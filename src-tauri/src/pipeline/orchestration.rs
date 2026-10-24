//! Recording-stop control flow independent of windows, devices and providers.
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
    fn capture(
        &self,
        mode: crate::state::PipelineMode,
        reason: StopReason,
        started: Instant,
    ) -> impl Future<Output = Result<Self::Captured, String>> + Send;
    fn transcription_message(&self, captured: &Self::Captured, reason: StopReason) -> &'static str;
    fn recognize(
        &self,
        id: u64,
        mode: crate::state::PipelineMode,
        captured: Self::Captured,
        cancel: watch::Receiver<bool>,
    ) -> impl Future<Output = Result<Self::Recognized, String>> + Send;
    fn deliver(
        &self,
        id: u64,
        mode: crate::state::PipelineMode,
        recognized: Self::Recognized,
    ) -> impl Future<Output = Result<RecordingResult, String>> + Send;
}

pub(super) async fn run(
    driver: &impl RecordingDriver,
    reason: StopReason,
) -> Result<RecordingResult, String> {
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
    driver.interactive(false);
    driver.stage(
        AppStage::FinishingAudio,
        processing_message(reason, false, "Stopping recording and preparing audio."),
    );
    let captured = driver.capture(mode, reason, Instant::now()).await?;
    if driver.lifecycle().is_cancelled(id) {
        driver.stage(AppStage::Idle, cancel_message);
        return Err(cancel_error.into());
    }
    driver.stage(
        AppStage::Transcribing,
        driver.transcription_message(&captured, reason),
    );
    let recognized = driver.recognize(id, mode, captured, cancel).await?;
    if driver.lifecycle().is_cancelled(id) {
        driver.stage(AppStage::Idle, cancel_message);
        return Err(cancel_error.into());
    }
    driver.deliver(id, mode, recognized).await
}

#[cfg(test)]
mod tests;
