//! Recording-stop control flow independent of windows, devices and providers.
use super::take_trace::TakeTrace;
use crate::storage::TakeStage;
use crate::{
    commands::{cancellation_copy, processing_message, StopReason},
    state::{AppState, PipelineLifecycle, TerminalPhaseGuard},
    types::{AppStage, RecordingResult},
    PipelineGuard,
};
use std::{future::Future, time::Instant};
use tokio::sync::watch;

/// The native adapter and test emitter both publish the same typed phases.
pub(super) trait RecordingEvents: Sync {
    fn state(&self) -> &AppState;
    fn stage(&self, stage: AppStage, message: &str);
    fn interactive(&self, interactive: bool);
}

/// Ports around capture, recognition and mode-specific delivery. The shared
/// runner owns lifecycle claims, cancellation boundaries and terminal guards.
pub(super) trait RecordingDriver: RecordingEvents {
    type Captured: Send;
    type Recognized: Send;
    fn lifecycle(&self) -> &PipelineLifecycle;
    /// The span trace of the stopped take `id` (disabled when nothing may be
    /// recorded, as for dictation tests).
    fn begin_trace(&self, id: u64, mode: crate::state::PipelineMode) -> TakeTrace;
    fn capture(
        &self,
        mode: crate::state::PipelineMode,
        reason: StopReason,
        started: Instant,
        trace: &TakeTrace,
    ) -> impl Future<Output = Result<Self::Captured, String>> + Send;
    fn transcription_message(&self, captured: &Self::Captured, reason: StopReason) -> &'static str;
    fn recognize(
        &self,
        id: u64,
        mode: crate::state::PipelineMode,
        captured: Self::Captured,
        cancel: watch::Receiver<bool>,
        trace: &TakeTrace,
    ) -> impl Future<Output = Result<Self::Recognized, String>> + Send;
    fn deliver(
        &self,
        id: u64,
        mode: crate::state::PipelineMode,
        recognized: Self::Recognized,
        trace: &TakeTrace,
    ) -> impl Future<Output = Result<RecordingResult, String>> + Send;
}

pub(super) async fn run(
    driver: &impl RecordingDriver,
    reason: StopReason,
) -> Result<RecordingResult, String> {
    // Stop-relative take spans start at the stop request.
    let stop_requested = Instant::now();
    let claimed = match reason {
        StopReason::HoldRelease(id) | StopReason::DictationTest(id) => {
            driver.lifecycle().begin_processing_for(Some(id))
        }
        _ => driver.lifecycle().begin_processing(),
    };
    let (id, mode, cancel) = claimed.map_err(|error| {
        // An automatic stop losing the race must not overwrite the winner.
        if error == "no recording is active" && reason == StopReason::User {
            driver.stage(
                AppStage::Idle,
                "No recording is active. Start a new recording.",
            );
        }
        error.to_owned()
    })?;
    let trace = driver.begin_trace(id, mode);
    trace.stopped(stop_requested);
    // Declared before the lifecycle guard, so it is dropped right after the
    // lifecycle accepts a new take: that instant ends stop to next take.
    let _release = trace.release_on_drop();
    let _owner = PipelineGuard {
        lifecycle: driver.lifecycle(),
        id,
    };
    let (cancel_message, cancel_error) = cancellation_copy(mode);
    let _terminal = TerminalPhaseGuard::new(
        driver.state(),
        driver.lifecycle(),
        id,
        cancel_message,
        |phase, message| driver.stage(AppStage::for_phase(&phase), message),
    );
    let result = async {
        driver.interactive(false);
        driver.stage(
            AppStage::FinishingAudio,
            processing_message(reason, false, "Stopping recording and preparing audio."),
        );
        trace.stage(TakeStage::Capture);
        let captured = driver.capture(mode, reason, Instant::now(), &trace).await?;
        if driver.lifecycle().is_cancelled(id) {
            driver.stage(AppStage::Idle, cancel_message);
            return Err(cancel_error.into());
        }
        driver.stage(
            AppStage::Transcribing,
            driver.transcription_message(&captured, reason),
        );
        trace.stage(TakeStage::Asr);
        let recognized = driver.recognize(id, mode, captured, cancel, &trace).await?;
        if driver.lifecycle().is_cancelled(id) {
            driver.stage(AppStage::Idle, cancel_message);
            return Err(cancel_error.into());
        }
        trace.stage(TakeStage::Ai);
        driver.deliver(id, mode, recognized, &trace).await
    }
    .await;
    trace.conclude(
        result
            .as_ref()
            .map(|result| result.insertion.as_str())
            .map_err(|_| ()),
        driver.lifecycle().is_cancelled(id),
    );
    result
}

#[cfg(test)]
mod tests;
