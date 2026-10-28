#![cfg(test)]
use super::*;
use crate::types::AppPhase;
use crate::{
    asr::{AsrError, Transcriber, Transcript, WorkerCommand},
    audio::{
        AudioArtifact, AudioCapture, AudioDevice, AudioError, AudioFuture, AudioSnapshot,
        CaptureConfig, CaptureState, LevelMeter, VadAnalysis,
    },
    state::{PipelineMode, PipelinePhase},
};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::Duration,
};

struct FakeAudio {
    fail: bool,
    stops: usize,
}
impl AudioCapture for FakeAudio {
    fn list_devices(&self) -> AudioFuture<'_, Vec<AudioDevice>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn arm(&mut self, _: CaptureConfig) -> AudioFuture<'_, ()> {
        Box::pin(async { unreachable!("stop cannot reopen the microphone") })
    }
    fn disarm(&mut self) -> AudioFuture<'_, ()> {
        Box::pin(async { unreachable!("stop owns stream teardown") })
    }
    fn start(&mut self, _: CaptureConfig) -> AudioFuture<'_, ()> {
        Box::pin(async { unreachable!("stop cannot restart recording") })
    }
    fn snapshot(&self, _: Duration) -> Result<AudioSnapshot, AudioError> {
        unreachable!()
    }
    fn stop(&mut self) -> AudioFuture<'_, AudioArtifact> {
        self.stops += 1;
        let fail = self.fail;
        Box::pin(async move {
            if fail {
                return Err(AudioError::NoVoiceDetected);
            }
            Ok(AudioArtifact {
                path: "fake.wav".into(),
                sample_rate: 24_000,
                channels: 1,
                sample_count: 24_000,
                duration: Duration::from_secs(1),
                peak_level: 0.2,
                rms_level: 0.1,
                vad: VadAnalysis::default(),
                warning: None,
            })
        })
    }
    fn cancel(&mut self) -> AudioFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn state(&self) -> CaptureState {
        CaptureState::Capturing
    }
    fn level(&self) -> LevelMeter {
        LevelMeter::default()
    }
}
struct FakeTranscriber {
    fail: bool,
    calls: AtomicUsize,
    wait_for_cancel: bool,
    entered: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl Transcriber for FakeTranscriber {
    async fn load(&self, _: &str) -> Result<(), AsrError> {
        Ok(())
    }
    async fn transcribe(
        &self,
        _: &Path,
        _: Option<&str>,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<Transcript, AsrError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.wait_for_cancel {
            self.entered.notify_one();
            let _ = cancel.wait_for(|cancelled| *cancelled).await;
            return Err(AsrError::Cancelled);
        }
        if self.fail {
            return Err(AsrError::Crashed);
        }
        Ok(Transcript {
            text: "recognized".into(),
            segments: vec![],
            model: "fake".into(),
            duration_ms: 1000,
        })
    }
    async fn shutdown(&self) -> Result<(), AsrError> {
        Ok(())
    }
    async fn reconfigure(&self, _: WorkerCommand) {}
}
#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    Capture,
    Recognition,
    Delivery,
    CancelCapture,
    CancelRecognition,
    CancelInFlight,
}
struct FakeDriver {
    lifecycle: PipelineLifecycle,
    state: AppState,
    audio: tokio::sync::Mutex<FakeAudio>,
    transcriber: FakeTranscriber,
    events: Mutex<Vec<AppStage>>,
    deliveries: Mutex<Vec<PipelineMode>>,
    fault: Fault,
}
impl FakeDriver {
    fn new(mode: PipelineMode, fault: Fault) -> Self {
        let lifecycle = PipelineLifecycle::default();
        let id = lifecycle.begin_start(mode).unwrap();
        lifecycle.mark_recording(id).unwrap();
        let state = AppState::default();
        state.transition_stage(AppStage::Recording, None);
        Self {
            lifecycle,
            state,
            audio: tokio::sync::Mutex::new(FakeAudio {
                fail: fault == Fault::Capture,
                stops: 0,
            }),
            transcriber: FakeTranscriber {
                fail: fault == Fault::Recognition,
                calls: AtomicUsize::new(0),
                wait_for_cancel: fault == Fault::CancelInFlight,
                entered: tokio::sync::Notify::new(),
            },
            events: Mutex::new(vec![]),
            deliveries: Mutex::new(vec![]),
            fault,
        }
    }
}
impl RecordingEvents for FakeDriver {
    fn state(&self) -> &AppState {
        &self.state
    }
    fn stage(&self, stage: AppStage, message: &str) {
        self.state.transition_stage(stage, Some(message.into()));
        self.events.lock().unwrap().push(stage);
    }
    fn interactive(&self, interactive: bool) {
        assert!(!interactive);
    }
}
impl RecordingDriver for FakeDriver {
    type Captured = AudioArtifact;
    type Recognized = Transcript;
    fn lifecycle(&self) -> &PipelineLifecycle {
        &self.lifecycle
    }
    async fn capture(
        &self,
        _: PipelineMode,
        _: StopReason,
        _: Instant,
    ) -> Result<AudioArtifact, String> {
        let artifact = self
            .audio
            .lock()
            .await
            .stop()
            .await
            .map_err(|e| e.to_string())?;
        if self.fault == Fault::CancelCapture {
            self.lifecycle.cancel().unwrap();
        }
        Ok(artifact)
    }
    fn transcription_message(&self, _: &AudioArtifact, _: StopReason) -> &'static str {
        "Transcribing locally."
    }
    async fn recognize(
        &self,
        _: u64,
        _: PipelineMode,
        captured: AudioArtifact,
        cancel: watch::Receiver<bool>,
    ) -> Result<Transcript, String> {
        let transcript = self
            .transcriber
            .transcribe(&captured.path, None, cancel)
            .await
            .map_err(|e| e.to_string())?;
        if self.fault == Fault::CancelRecognition {
            self.lifecycle.cancel().unwrap();
        }
        Ok(transcript)
    }
    async fn deliver(
        &self,
        _: u64,
        mode: PipelineMode,
        transcript: Transcript,
    ) -> Result<RecordingResult, String> {
        self.deliveries.lock().unwrap().push(mode);
        if self.fault == Fault::Delivery {
            return Err("delivery failed".into());
        }
        self.state.complete(
            transcript.text.clone(),
            "Dictation inserted successfully.".into(),
        );
        self.events.lock().unwrap().push(AppStage::Completed);
        Ok(RecordingResult {
            text: transcript.text,
            insertion: "fake".into(),
            duration_ms: 1000,
            latency_ms: 1,
        })
    }
}
const MODES: [PipelineMode; 4] = [
    PipelineMode::Dictate,
    PipelineMode::Translate,
    PipelineMode::Edit,
    PipelineMode::Ask,
];
#[tokio::test]
async fn all_recording_modes_stop_recognize_dispatch_and_release_the_lifecycle() {
    for mode in MODES {
        let driver = FakeDriver::new(mode, Fault::None);
        assert_eq!(
            run(&driver, StopReason::User).await.unwrap().text,
            "recognized"
        );
        assert_eq!(
            *driver.events.lock().unwrap(),
            [
                AppStage::FinishingAudio,
                AppStage::Transcribing,
                AppStage::Completed
            ]
        );
        assert_eq!(*driver.deliveries.lock().unwrap(), [mode]);
        assert_eq!(driver.audio.lock().await.stops, 1);
        assert_eq!(driver.transcriber.calls.load(Ordering::SeqCst), 1);
        assert_eq!(driver.lifecycle.phase(), PipelinePhase::Idle);
    }
}
#[tokio::test]
async fn errors_at_each_stage_publish_terminal_error_and_release_every_mode() {
    for mode in MODES {
        for fault in [Fault::Capture, Fault::Recognition, Fault::Delivery] {
            let driver = FakeDriver::new(mode, fault);
            assert!(run(&driver, StopReason::User).await.is_err());
            assert_eq!(driver.state.snapshot().phase, AppPhase::Error);
            assert_eq!(driver.lifecycle.phase(), PipelinePhase::Idle);
            assert_eq!(
                driver.transcriber.calls.load(Ordering::SeqCst),
                usize::from(fault != Fault::Capture)
            );
            assert_eq!(
                driver.deliveries.lock().unwrap().len(),
                usize::from(fault == Fault::Delivery)
            );
        }
    }
}
#[tokio::test]
async fn cancellation_after_capture_or_recognition_never_delivers() {
    for mode in MODES {
        for fault in [Fault::CancelCapture, Fault::CancelRecognition] {
            let driver = FakeDriver::new(mode, fault);
            assert!(run(&driver, StopReason::User).await.is_err());
            assert_eq!(driver.state.snapshot().phase, AppPhase::Idle);
            assert_eq!(driver.lifecycle.phase(), PipelinePhase::Idle);
            assert!(driver.deliveries.lock().unwrap().is_empty());
            assert_eq!(
                driver.transcriber.calls.load(Ordering::SeqCst),
                usize::from(fault == Fault::CancelRecognition)
            );
        }
    }
}
#[tokio::test]
async fn a_losing_automatic_stop_does_not_replace_completed_state() {
    let driver = FakeDriver::new(PipelineMode::Dictate, Fault::None);
    run(&driver, StopReason::User).await.unwrap();
    let before = driver.state.snapshot();
    let events = driver.events.lock().unwrap().len();
    assert!(run(&driver, StopReason::MaximumDuration).await.is_err());
    assert_eq!(driver.state.snapshot(), before);
    assert_eq!(driver.events.lock().unwrap().len(), events);
    assert_eq!(driver.audio.lock().await.stops, 1);
}

#[tokio::test]
async fn stale_hold_release_does_not_touch_another_recording() {
    let driver = FakeDriver::new(PipelineMode::Dictate, Fault::None);
    let current = driver.lifecycle.begin_processing().unwrap().0;
    driver.lifecycle.finish(current);
    let next = driver.lifecycle.begin_start(PipelineMode::Ask).unwrap();
    driver.lifecycle.mark_recording(next).unwrap();
    assert!(run(&driver, StopReason::HoldRelease(current))
        .await
        .is_err());
    assert_eq!(driver.lifecycle.phase(), PipelinePhase::Recording);
    assert_eq!(driver.audio.lock().await.stops, 0);
    assert_eq!(driver.transcriber.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelling_in_flight_recognition_publishes_idle_without_delivery() {
    for mode in MODES {
        let driver = FakeDriver::new(mode, Fault::CancelInFlight);
        let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(run(&driver, StopReason::User), async {
                driver.transcriber.entered.notified().await;
                driver.lifecycle.cancel().unwrap();
            })
        })
        .await
        .expect("cancel must interrupt recognition");
        assert!(result.is_err());
        assert_eq!(
            *driver.events.lock().unwrap(),
            [
                AppStage::FinishingAudio,
                AppStage::Transcribing,
                AppStage::Idle
            ]
        );
        assert!(driver.deliveries.lock().unwrap().is_empty());
        assert_eq!(driver.lifecycle.phase(), PipelinePhase::Idle);
    }
}
