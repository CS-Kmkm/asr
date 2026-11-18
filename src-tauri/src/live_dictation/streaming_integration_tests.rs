//! Explicit application-owner smoke; no microphone, UI input or downloads.
use super::*;
use crate::{
    asr::JsonlTranscriber,
    audio::{
        AudioArtifact, AudioDevice, AudioError, AudioFuture, AudioSnapshot, CaptureState,
        LevelMeter,
    },
    types::AsrDevice,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
};

struct SnapshotCapture {
    samples: Arc<Vec<f32>>,
    config: CaptureConfig,
    tail: Arc<StdMutex<Option<FinalTail>>>,
}

impl AudioCapture for SnapshotCapture {
    fn list_devices(&self) -> AudioFuture<'_, Vec<AudioDevice>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn arm(&mut self, _: CaptureConfig) -> AudioFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn disarm(&mut self) -> AudioFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn start(&mut self, _: CaptureConfig) -> AudioFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn snapshot(&self, since: Duration) -> Result<AudioSnapshot, AudioError> {
        let end = self.samples.len() * 3 / 4;
        let from = (since.as_secs_f64() * f64::from(audio::TARGET_SAMPLE_RATE)).round() as usize;
        Ok(AudioSnapshot::for_test(
            self.samples[from.min(end)..end].to_vec(),
            self.config.clone(),
        ))
    }
    fn retain_final_tail(&mut self, tail: FinalTail) -> bool {
        *self.tail.lock().unwrap() = Some(tail);
        true
    }
    fn stop(&mut self) -> AudioFuture<'_, AudioArtifact> {
        panic!("fixture uses a sealed stop tail, not a microphone")
    }
    fn cancel(&mut self) -> AudioFuture<'_, ()> {
        panic!("stream owner must not cancel capture")
    }
    fn state(&self) -> CaptureState {
        CaptureState::Capturing
    }
    fn level(&self) -> LevelMeter {
        LevelMeter::default()
    }
}

/// Submit the first 3/4 through the production recording loop, then seal the
/// remainder as capture stop does. Deliberately accelerated, not a timing test.
async fn recorded_session(
    asr: &dyn Transcriber,
    settings: &Settings,
    samples: &[f32],
) -> Result<Session, AsrError> {
    let config = CaptureConfig::default();
    let tail = Arc::new(StdMutex::new(None));
    let capture: Mutex<Box<dyn AudioCapture>> = Mutex::new(Box::new(SnapshotCapture {
        samples: Arc::new(samples.to_vec()),
        config: config.clone(),
        tail: tail.clone(),
    }));
    let mut session =
        Session::plan(settings).expect("fixture requires Nemotron streaming settings");
    let (_cancel, cancelled) = watch::channel(false);
    let (stop, stopped) = watch::channel(false);
    session
        .run(&capture, asr, cancelled, stopped, |_| {
            tail.lock()
                .unwrap()
                .as_ref()
                .expect("capture tail registered")
                .seal(samples, audio::TARGET_SAMPLE_RATE, &config);
            stop.send_replace(true);
        })
        .await;
    if session.failed || session.cursor != samples.len() * 3 / 4 {
        return Err(AsrError::Protocol(
            "recording-owner fixture did not reach its stop boundary".into(),
        ));
    }
    Ok(session)
}

fn settings(device: AsrDevice) -> Settings {
    Settings {
        asr_backend: "sherpa-onnx".into(),
        model_id: Some(MODEL.into()),
        asr_device: Some(device),
        stateful_streaming: true,
        offline_only: true,
        speech_locale: Some("ja-JP".into()),
        ..Settings::default()
    }
}

async fn exercise(
    asr: &dyn Transcriber,
    settings: &Settings,
    samples: &[f32],
) -> Result<serde_json::Value, AsrError> {
    let (_cancel, cancelled) = watch::channel(false);
    let first = recorded_session(asr, settings, samples)
        .await?
        .finish(asr, settings, cancelled.clone())
        .await?;
    let changed = Settings {
        input_gain_percent: settings.input_gain_percent + 1,
        ..settings.clone()
    };
    let rejected = recorded_session(asr, settings, samples)
        .await?
        .finish(asr, &changed, cancelled.clone())
        .await;
    if !matches!(rejected, Err(AsrError::Protocol(ref message)) if message == "stream final reuse unavailable")
    {
        return Err(AsrError::Protocol(
            "changed settings were not rejected before final reuse".into(),
        ));
    }
    // A new session must work after the rejected session's stream_close.
    let next = recorded_session(asr, settings, samples)
        .await?
        .finish(asr, settings, cancelled)
        .await?;
    if first.text.trim().is_empty()
        || next.text.trim().is_empty()
        || first.model != format!("sherpa-onnx:{MODEL}")
        || next.model != first.model
    {
        return Err(AsrError::Protocol(
            "Nemotron final text or model identity is invalid".into(),
        ));
    }
    Ok(
        json!({"first_nonempty_final": true, "changed_settings_rejected": true,
        "subsequent_nonempty_final": true, "scope": "Rust recording owner; fixture capture, no GUI or insertion"}),
    )
}

#[tokio::test]
async fn application_owner_seals_stop_tail_and_reopens_after_rejected_reuse() {
    let asr = super::tests::FakeAsr::new();
    let samples = vec![0.1; 24_000];
    let report = exercise(&asr, &settings(AsrDevice::Cpu), &samples)
        .await
        .unwrap();
    assert_eq!(report["changed_settings_rejected"], true);
    assert_eq!(report["subsequent_nonempty_final"], true);
    let requests = asr.requests.lock().unwrap();
    let starts: Vec<_> = requests
        .iter()
        .enumerate()
        .filter_map(|(index, request)| (request["command"] == "stream_start").then_some(index))
        .collect();
    assert_eq!(starts.len(), 3);
    let rejected = &requests[starts[1]..starts[2]];
    assert!(rejected
        .iter()
        .all(|request| request["command"] != "stream_flush"));
    let close = rejected.last().unwrap();
    assert_eq!(close["command"], "stream_close");
    assert_eq!(close["session_id"], rejected[0]["session_id"]);
}

#[tokio::test]
#[ignore = "requires ASR_TEST_NEMOTRON_PYTHON, ASR_TEST_NEMOTRON_WAV, ASR_TEST_NEMOTRON_DEVICE and cached pinned files"]
async fn cached_nemotron_application_owner() {
    let required = |name| {
        std::env::var_os(name)
            .unwrap_or_else(|| panic!("set {name} before explicitly running this ignored test"))
    };
    let python = PathBuf::from(required("ASR_TEST_NEMOTRON_PYTHON"));
    let wav = PathBuf::from(required("ASR_TEST_NEMOTRON_WAV"));
    let device = match required("ASR_TEST_NEMOTRON_DEVICE").to_str() {
        Some("cpu") => AsrDevice::Cpu,
        Some("cuda") => AsrDevice::Cuda,
        _ => panic!("ASR_TEST_NEMOTRON_DEVICE must be cpu or cuda"),
    };
    assert!(
        python.is_file(),
        "ASR_TEST_NEMOTRON_PYTHON must name an existing interpreter"
    );
    let (samples, rate) =
        audio::read_pcm16_wav(&wav).expect("ASR_TEST_NEMOTRON_WAV must be mono PCM16 WAV");
    assert!(
        samples.len() >= rate as usize,
        "provide at least one second of speech"
    );
    let samples = audio::resample_linear(&samples, rate, audio::TARGET_SAMPLE_RATE);
    let settings = settings(device);
    let mut command = crate::worker_command_with_speech_key(&settings, None);
    command.program = python;
    command.provision = None;
    command.source_required = true;
    command.current_dir = Some(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_owned(),
    );
    let asr = JsonlTranscriber::new(command, Duration::from_secs(30), Duration::from_secs(120));
    let result = tokio::time::timeout(Duration::from_secs(180), async {
        asr.load(&settings.model_quantization).await?;
        let health = asr.health().await?;
        let mut report = exercise(&asr, &settings, &samples).await?;
        report["backend"] = json!(health.backend);
        report["device"] = json!(health.device);
        report["runtime"] = json!(health.runtime);
        report["device_name"] = json!(health.device_name);
        Ok::<_, AsrError>(report)
    })
    .await;
    let shutdown = asr.shutdown().await;
    shutdown.expect("real worker must shut down after the integration attempt");
    let report = result
        .expect("application-owner smoke timed out")
        .expect("application-owner smoke failed");
    assert_eq!(report["device"], device.as_str());
    assert_eq!(report["backend"], "sherpa-onnx");
    eprintln!("nemotron_app_integration={report}");
}
