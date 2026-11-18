//! Opt-in online inference. Each raw sample is submitted once; final reuse
//! requires a sealed capture tail, unchanged settings and model generation.
use crate::{
    asr::{AsrError, ModelResidency, Transcriber, Transcript},
    audio::{self, AudioCapture, CaptureConfig, FinalTail},
    types::Settings,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{watch, Mutex};

pub(crate) const MODEL: &str = "nemotron-3.5-streaming-560ms";

pub(crate) fn enabled(settings: &Settings) -> bool {
    settings.stateful_streaming
        && ((settings.asr_backend == "sherpa-onnx" && settings.model_id.as_deref() == Some(MODEL))
            || (settings.asr_backend == "openai-compatible"
                && settings.qwen_demo_streaming
                && crate::privacy::live_preview_enabled(settings)))
}

fn generation(asr: &dyn Transcriber) -> Option<u64> {
    match *asr.model_residency()?.borrow() {
        ModelResidency::Resident(value) => Some(value),
        _ => None,
    }
}

/// Continuous linear resampling uses absolute rational positions, so neither
/// chunk boundaries nor different microphone rates duplicate/drop samples.
#[derive(Default)]
struct Resampler {
    pending: Vec<f32>,
    base: usize,
    total: usize,
    next: usize,
    rate: u32,
}

impl Resampler {
    fn push(
        &mut self,
        samples: Vec<f32>,
        rate: u32,
        final_chunk: bool,
    ) -> Result<Vec<f32>, AsrError> {
        if rate == 0 || (self.rate != 0 && self.rate != rate) {
            return Err(AsrError::Protocol("stream capture rate changed".into()));
        }
        self.rate = rate;
        self.total += samples.len();
        self.pending.extend(samples);
        let mut output = Vec::new();
        loop {
            let position = self.next as u64 * u64::from(rate);
            let left = (position / 16000) as usize;
            if left >= self.total || (!final_chunk && left + 1 >= self.total) {
                break;
            }
            let right = (left + 1).min(self.total - 1);
            let fraction = (position % 16000) as f32 / 16000.0;
            let a = self.pending[left - self.base];
            let b = self.pending[right - self.base];
            output.push(a + (b - a) * fraction);
            self.next += 1;
        }
        let keep = ((self.next as u64 * u64::from(rate) / 16000) as usize).min(self.total);
        self.pending.drain(..keep - self.base);
        self.base = keep;
        Ok(output)
    }
}

pub(crate) struct Session {
    id: String,
    settings: Settings,
    tail: FinalTail,
    cursor: usize,
    config: Option<CaptureConfig>,
    resampler: Resampler,
    generation: Option<u64>,
    offset: usize,
    started: bool,
    failed: bool,
}

impl Session {
    pub(crate) fn plan(settings: &Settings) -> Option<Self> {
        static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
        enabled(settings).then(|| Self {
            id: format!(
                "{}-{}",
                std::process::id(),
                NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
            ),
            settings: settings.clone(),
            tail: FinalTail::default(),
            cursor: 0,
            config: None,
            resampler: Resampler::default(),
            generation: None,
            offset: 0,
            started: false,
            failed: false,
        })
    }

    pub(crate) async fn run(
        &mut self,
        audio: &Mutex<Box<dyn AudioCapture>>,
        asr: &dyn Transcriber,
        mut cancel: watch::Receiver<bool>,
        mut stop: watch::Receiver<bool>,
        mut preview: impl FnMut(&str),
    ) {
        let result = self
            .run_inner(audio, asr, &mut cancel, &mut stop, &mut preview)
            .await;
        if result.is_err() || *cancel.borrow() {
            self.failed = true;
            self.close(asr).await;
        }
    }

    async fn run_inner(
        &mut self,
        audio: &Mutex<Box<dyn AudioCapture>>,
        asr: &dyn Transcriber,
        cancel: &mut watch::Receiver<bool>,
        stop: &mut watch::Receiver<bool>,
        preview: &mut impl FnMut(&str),
    ) -> Result<(), AsrError> {
        if !audio.lock().await.retain_final_tail(self.tail.clone()) {
            return Err(AsrError::Protocol(
                "stream capture coverage unavailable".into(),
            ));
        }
        self.generation = generation(asr);
        if self.generation.is_none() {
            return Err(AsrError::Protocol("stream model unverified".into()));
        }
        // Cleanup also attempts close if the start response is lost or invalid.
        self.started = true;
        let reply = asr
            .stream_request(
                json!({"command":"stream_start", "session_id":self.id, "sample_rate":16000, "language":self.settings.speech_locale}),
                cancel.clone(),
            )
            .await?;
        self.check(&reply, false)?;
        loop {
            if *cancel.borrow() {
                return Err(AsrError::Cancelled);
            }
            if *stop.borrow() {
                return Ok(());
            }
            let snapshot = {
                let capture = audio.lock().await;
                if *stop.borrow() {
                    return Ok(());
                }
                let since = if self.resampler.rate == 0 {
                    Duration::ZERO
                } else {
                    audio::raw_position(self.cursor, self.resampler.rate)
                };
                match capture.snapshot(since) {
                    Ok(value) => value,
                    Err(audio::AudioError::NotCapturing) => return Ok(()),
                    Err(_) => return Err(AsrError::Protocol("stream capture failed".into())),
                }
            };
            let count = snapshot.raw_len();
            if count > 0 {
                // Move the stop-time tail mark before releasing inference to the
                // worker; stop may seal it while this exact chunk is in flight.
                if !self.tail.advance(self.cursor + count) {
                    return Ok(());
                }
                self.cursor += count;
                let (samples, rate, config) = snapshot.into_stream_pcm();
                self.check_config(&config)?;
                let samples = self.resampler.push(samples, rate, false)?;
                let text = self.submit(asr, samples, cancel.clone()).await?;
                preview(&text);
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(150)) => {},
                _ = stop.changed() => {},
                _ = cancel.changed() => {},
            }
        }
    }

    fn check_config(&mut self, config: &CaptureConfig) -> Result<(), AsrError> {
        if self
            .config
            .as_ref()
            .is_some_and(|old| !old.prepares_like(config))
        {
            return Err(AsrError::Protocol("stream preparation changed".into()));
        }
        self.config.get_or_insert_with(|| config.clone());
        Ok(())
    }

    fn check(&self, reply: &Value, final_result: bool) -> Result<(), AsrError> {
        if reply["session_id"].as_str() != Some(&self.id)
            || reply["offset"].as_u64() != Some(self.offset as u64)
            || reply["model"].as_str() != Some(self.expected_model().as_str())
            || reply["final"].as_bool() != Some(final_result)
            || reply["text"].as_str().is_none()
        {
            return Err(AsrError::Protocol("invalid streaming response".into()));
        }
        Ok(())
    }

    fn expected_model(&self) -> String {
        if self.settings.asr_backend == "openai-compatible" {
            "qwen-demo:unverified".into()
        } else {
            format!("sherpa-onnx:{MODEL}")
        }
    }

    async fn submit(
        &mut self,
        asr: &dyn Transcriber,
        samples: Vec<f32>,
        cancel: watch::Receiver<bool>,
    ) -> Result<String, AsrError> {
        let mut text = String::new();
        for chunk in samples.chunks(32000) {
            let reply = asr
                .stream_request(
                    json!({"command":"stream_push", "session_id":self.id,
                "offset":self.offset, "samples":chunk}),
                    cancel.clone(),
                )
                .await?;
            self.offset += chunk.len();
            self.check(&reply, false)?;
            text = reply["text"].as_str().unwrap().to_owned();
            if generation(asr) != self.generation {
                return Err(AsrError::Protocol("stream model changed".into()));
            }
        }
        Ok(text)
    }

    pub(crate) async fn finish(
        mut self,
        asr: &dyn Transcriber,
        settings: &Settings,
        cancel: watch::Receiver<bool>,
    ) -> Result<Transcript, AsrError> {
        let result = self.finish_inner(asr, settings, cancel).await;
        self.close(asr).await;
        result
    }

    async fn finish_inner(
        &mut self,
        asr: &dyn Transcriber,
        settings: &Settings,
        cancel: watch::Receiver<bool>,
    ) -> Result<Transcript, AsrError> {
        if *cancel.borrow() {
            return Err(AsrError::Cancelled);
        }
        let old = &self.settings;
        if self.failed
            || !self.started
            || !enabled(settings)
            || generation(asr) != self.generation
            || old.model_id != settings.model_id
            || old.asr_device != settings.asr_device
            || old.model_quantization != settings.model_quantization
            || old.speech_locale != settings.speech_locale
            || old.input_gain_percent != settings.input_gain_percent
            || old.microphone_id != settings.microphone_id
            || old.qwen_demo_streaming != settings.qwen_demo_streaming
            || old.api_base_url != settings.api_base_url
        {
            return Err(AsrError::Protocol("stream final reuse unavailable".into()));
        }
        let tail = self
            .tail
            .take()
            .ok_or_else(|| AsrError::Protocol("stream tail not sealed".into()))?;
        if tail.from != self.cursor || tail.total < self.cursor || self.cursor == 0 {
            return Err(AsrError::Protocol("stream coverage incomplete".into()));
        }
        let snapshot = tail
            .audio
            .ok_or_else(|| AsrError::Protocol("stream tail missing".into()))?;
        if snapshot.raw_len() != tail.total - self.cursor {
            return Err(AsrError::Protocol("stream tail truncated".into()));
        }
        let (samples, rate, config) = snapshot.into_stream_pcm();
        self.check_config(&config)?;
        let samples = self.resampler.push(samples, rate, true)?;
        self.submit(asr, samples, cancel.clone()).await?;
        let reply = asr
            .stream_request(
                json!({"command":"stream_flush", "session_id":self.id, "offset":self.offset}),
                cancel,
            )
            .await?;
        self.check(&reply, true)?;
        if reply["reusable"].as_bool() == Some(false) {
            return Err(AsrError::Protocol(
                "stream service final metadata is unverified; use full audio".into(),
            ));
        }
        if generation(asr) != self.generation {
            return Err(AsrError::Protocol("stream model changed at flush".into()));
        }
        let text = reply["text"].as_str().unwrap().trim().to_owned();
        if text.is_empty() {
            return Err(AsrError::Protocol("stream final result empty".into()));
        }
        Ok(Transcript {
            text,
            segments: vec![],
            model: reply["model"].as_str().unwrap().into(),
            duration_ms: 0,
        })
    }

    async fn close(&self, asr: &dyn Transcriber) {
        if self.started {
            let (_, cancel) = watch::channel(false);
            let _ = asr
                .stream_request(
                    json!({"command":"stream_close", "session_id":self.id}),
                    cancel,
                )
                .await;
        }
    }
}

#[cfg(test)]
#[path = "streaming_integration_tests.rs"]
mod integration_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen_without_preview_does_not_send_audio_during_recording() {
        let mut settings = Settings {
            asr_backend: "openai-compatible".into(),
            qwen_demo_streaming: true,
            stateful_streaming: true,
            ..Settings::default()
        };
        assert!(!enabled(&settings));
        assert!(Session::plan(&settings).is_none());
        settings.cloud_live_preview_enabled = true;
        assert!(enabled(&settings));
        assert!(Session::plan(&settings).is_some());
    }
    use async_trait::async_trait;
    use std::{path::Path, sync::Mutex as StdMutex};

    pub(super) struct FakeAsr {
        residency: watch::Sender<ModelResidency>,
        pub(super) requests: StdMutex<Vec<Value>>,
        bad_flush: bool,
    }
    impl FakeAsr {
        pub(super) fn new() -> Self {
            Self {
                residency: watch::channel(ModelResidency::Resident(1)).0,
                requests: StdMutex::new(vec![]),
                bad_flush: false,
            }
        }
    }
    #[async_trait]
    impl Transcriber for FakeAsr {
        async fn load(&self, _: &str) -> Result<(), AsrError> {
            Ok(())
        }
        async fn transcribe(
            &self,
            _: &Path,
            _: Option<&str>,
            _: watch::Receiver<bool>,
        ) -> Result<Transcript, AsrError> {
            panic!("session cannot perform batch recognition")
        }
        async fn shutdown(&self) -> Result<(), AsrError> {
            Ok(())
        }
        async fn reconfigure(&self, _: crate::asr::WorkerCommand) {}
        fn model_residency(&self) -> Option<watch::Receiver<ModelResidency>> {
            Some(self.residency.subscribe())
        }
        async fn stream_request(
            &self,
            request: Value,
            cancel: watch::Receiver<bool>,
        ) -> Result<Value, AsrError> {
            if *cancel.borrow() {
                return Err(AsrError::Cancelled);
            }
            let final_result = request["command"] == "stream_flush";
            self.requests.lock().unwrap().push(request.clone());
            let offset = request["offset"].as_u64().unwrap_or(0)
                + request["samples"]
                    .as_array()
                    .map_or(0, |values| values.len() as u64);
            Ok(
                json!({"session_id":request["session_id"], "offset":offset, "model":"sherpa-onnx:nemotron-3.5-streaming-560ms",
                "text": if final_result { "確定した文章" } else { "途中の文章" }, "final":final_result && !self.bad_flush}),
            )
        }
    }

    fn recorded() -> (Session, Settings, FakeAsr) {
        let settings = Settings {
            stateful_streaming: true,
            asr_backend: "sherpa-onnx".into(),
            model_id: Some(MODEL.into()),
            ..Settings::default()
        };
        let mut session = Session::plan(&settings).unwrap();
        let config = CaptureConfig::default();
        session.config = Some(config.clone());
        session.cursor = 2400;
        session.generation = Some(1);
        session.started = true;
        session
            .resampler
            .push(vec![0.1; 2400], 24000, false)
            .unwrap();
        session.offset = session.resampler.next;
        session.tail.advance(2400);
        session.tail.seal(&vec![0.1; 4800], 24000, &config);
        (session, settings, FakeAsr::new())
    }

    #[tokio::test]
    async fn sealed_tail_is_sent_once_before_a_verified_flush() {
        let (session, settings, asr) = recorded();
        let offset = session.offset;
        let (_, cancel) = watch::channel(false);
        let result = session.finish(&asr, &settings, cancel).await.unwrap();
        assert_eq!(result.text, "確定した文章");
        let requests = asr.requests.lock().unwrap();
        let sent: usize = requests
            .iter()
            .filter_map(|r| r["samples"].as_array().map(Vec::len))
            .sum();
        assert_eq!(offset + sent, 3200);
        assert_eq!(requests[0]["offset"], offset);
        let flush = requests
            .iter()
            .find(|r| r["command"] == "stream_flush")
            .unwrap();
        assert_eq!(flush["offset"], 3200);
    }

    #[tokio::test]
    async fn incomplete_coverage_settings_generation_and_bad_flush_never_reuse() {
        for failure in [
            "coverage",
            "settings",
            "generation",
            "flush",
            "empty",
            "cancel",
        ] {
            let (mut session, mut settings, mut asr) = recorded();
            let (signal, cancel) = watch::channel(false);
            match failure {
                "coverage" => session.cursor += 1,
                "settings" => settings.input_gain_percent += 1,
                "generation" => {
                    asr.residency.send_replace(ModelResidency::Resident(2));
                }
                "flush" => asr.bad_flush = true,
                "empty" => session.started = false,
                "cancel" => {
                    signal.send_replace(true);
                }
                _ => unreachable!(),
            }
            assert!(
                session.finish(&asr, &settings, cancel).await.is_err(),
                "{failure}"
            );
            if failure != "flush" {
                assert!(!asr
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r["command"] == "stream_flush"));
            }
        }
    }
    #[test]
    fn resampling_is_independent_of_chunk_boundaries() {
        for rate in [16000, 24000, 44100, 48000] {
            let input: Vec<f32> = (0..10003).map(|i| (i as f32 * 0.03).sin()).collect();
            let expected = Resampler::default()
                .push(input.clone(), rate, true)
                .unwrap();
            let mut resampler = Resampler::default();
            let mut actual = vec![];
            for chunk in input.chunks(731) {
                actual.extend(resampler.push(chunk.to_vec(), rate, false).unwrap());
            }
            actual.extend(resampler.push(vec![], rate, true).unwrap());
            assert_eq!(expected, actual);
            assert!(resampler.pending.len() <= 1);
        }
    }
    #[test]
    fn streaming_is_explicit_and_model_specific() {
        let mut settings = Settings::default();
        assert!(!enabled(&settings));
        settings.stateful_streaming = true;
        assert!(!enabled(&settings));
        settings.asr_backend = "sherpa-onnx".into();
        settings.model_id = Some(MODEL.into());
        assert!(enabled(&settings));
    }
}
