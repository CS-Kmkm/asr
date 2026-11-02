use crate::{
    asr::{self, Transcriber},
    audio::{self, AudioCapture, AudioError},
    command_error, emit_status,
    injection::{
        self, InjectionOptions, InsertResult, InsertionDetail, ProvisionalInsertion,
        SystemTextInjector, TargetWindow,
    },
    input_monitor::InputMonitor,
    recording_overlay,
    services::Services,
    state::{AppState, PipelineMode, PipelinePhase},
    types::Settings,
    TempArtifact,
};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::watch;
#[cfg(test)]
mod benchmark;
pub(crate) mod incremental;
mod segmentation;
use segmentation::{append_utterance, trailing_quiet, Decision, Segmentation};

const LIVE_INTERVAL: Duration = Duration::from_millis(150);

/// Skips copying and enhancing the whole current utterance while the audio
/// captured since the last full preparation cannot change its `Wait`.
///
/// A preparation runs whenever enough new audio may be due for a decode, the
/// new audio could complete the shortest endpoint silence, or `MAX_DEFERRAL`
/// of audio has arrived since the last one. Enhancement depends on
/// whole-window statistics, so nothing prepared is reused: a preparation that
/// runs sees exactly the audio it saw before this gate existed.
#[derive(Default)]
struct PreparationGate {
    /// The window (raw audio since the cursor) at the last preparation that
    /// ended in `Wait`, and the silence trailing its last voiced frame.
    waited: Option<(Duration, Duration)>,
}

impl PreparationGate {
    /// The longest audio a gate lets accumulate without a preparation.
    const MAX_DEFERRAL: Duration = Duration::from_millis(1_000);
    /// Slack for resampling rounding and frame alignment.
    const TOLERANCE: Duration = Duration::from_millis(40);

    /// Where the audio the last preparation did not see starts, relative to
    /// the cursor; `None` when the next iteration must prepare anyway.
    fn unseen_from(&self) -> Option<Duration> {
        self.waited.map(|(window, _)| window)
    }

    fn should_prepare(&self, probe: &audio::AudioProbe, segmentation: &Segmentation) -> bool {
        let Some((window, quiet)) = self.waited else {
            return true;
        };
        let new = probe.duration().saturating_sub(window);
        if new.is_zero() {
            // The same audio prepares and plans exactly as before.
            return false;
        }
        if new >= Self::MAX_DEFERRAL || segmentation.may_decode(probe.duration(), Self::TOLERANCE) {
            return true;
        }
        // Silence now trails the last certainly voiced new frame, or adds to
        // the silence the last preparation ended with.
        let quiet = probe
            .quiet_after_voice(segmentation::FRAME, segmentation::VOICE_HYSTERESIS as f32)
            .unwrap_or(quiet + new);
        quiet + Self::TOLERANCE >= segmentation.shortest_endpoint_silence(probe.duration())
    }

    fn waited(&mut self, window: Duration, trailing_quiet: Duration) {
        self.waited = Some((window, trailing_quiet));
    }

    fn reset(&mut self) {
        self.waited = None;
    }
}

/// Default Dictate pastes only the final text once. Live provisional target
/// insertion is an explicit opt-in because editors whose content cannot be
/// read back keep the first unverified fragment. Other modes always defer.
/// The cheaper greedy decode is an explicit opt-in for live preview only.
fn preview_decoding(settings: &Settings) -> asr::PreviewDecoding {
    if settings.live_preview_fast_decode {
        asr::PreviewDecoding::Greedy
    } else {
        asr::PreviewDecoding::Standard
    }
}

pub(crate) fn defers_target_insertion(mode: PipelineMode, settings: &Settings) -> bool {
    mode != PipelineMode::Dictate
        || settings.automatic_voice_mode
        || !settings.live_target_insertion
}

#[derive(Clone, Copy)]
struct SettleTiming {
    quiet: Duration,
    limit: Duration,
    poll: Duration,
    /// How long a quiet target may stay unready before the user is told
    /// what the paste is waiting for.
    hint: Duration,
}

/// Clicking, scrolling or typing during recording and recognition is normal.
/// A deferred final paste waits for a short quiet moment instead.
const SETTLE_TIMING: SettleTiming = SettleTiming {
    quiet: Duration::from_millis(350),
    limit: Duration::from_secs(5),
    poll: Duration::from_millis(50),
    hint: Duration::from_secs(1),
};

/// The result of [`settled_checkpoint`].
#[derive(Debug, PartialEq, Eq)]
struct Settled {
    checkpoint: Option<u64>,
    /// Whether it slept at least once, as opposed to settling on the first check.
    waited: bool,
}

/// Returns a checkpoint taken once input has been quiet for `timing.quiet`,
/// preferring a moment when `ready` also holds. Quiet time counts from the
/// last input the monitor observed, so input that ended before the wait adds
/// no delay; a held shortcut or modifier is never quiet. At the limit, a
/// quiet but unready target still yields a checkpoint so the outcome names
/// the target as the reason. No checkpoint means input never settled, the
/// monitor stopped, or the operation was cancelled; the caller keeps its
/// earlier checkpoint. Once a quiet target has stayed unready for
/// `timing.hint`, `on_unready` runs once so the user can act instead of
/// waiting silently.
async fn settled_checkpoint(
    monitor: &InputMonitor,
    cancel: &watch::Receiver<bool>,
    timing: SettleTiming,
    ready: impl Fn() -> bool,
    on_unready: impl FnOnce(),
) -> Settled {
    let started = Instant::now();
    let mut quiet_since = monitor.last_input();
    let quiet = |since: Option<Instant>| since.is_none_or(|at| at.elapsed() >= timing.quiet);
    let mut unready_since = None;
    let mut on_unready = Some(on_unready);
    let mut waited = false;
    let settled = |checkpoint, waited| Settled { checkpoint, waited };
    loop {
        if *cancel.borrow() {
            return settled(None, waited);
        }
        let Some(mut current) = monitor.checkpoint() else {
            return settled(None, waited);
        };
        // The monitor stamps input before publishing its sequence, so this
        // is at least as recent as any input `current` includes.
        quiet_since = quiet_since.max(monitor.last_input());
        if monitor.shortcut_pending() {
            quiet_since = Some(Instant::now());
        }
        if quiet(quiet_since) {
            if ready() {
                // Input during the readiness check would make this checkpoint stale.
                let Some(after) = monitor.checkpoint() else {
                    return settled(None, waited);
                };
                if after == current {
                    return settled(Some(current), waited);
                }
                current = after;
                quiet_since = quiet_since.max(monitor.last_input());
                unready_since = None;
            } else if unready_since.get_or_insert_with(Instant::now).elapsed() >= timing.hint {
                if let Some(on_unready) = on_unready.take() {
                    on_unready();
                }
            }
        }
        if started.elapsed() >= timing.limit {
            return settled(quiet(quiet_since).then_some(current), waited);
        }
        tokio::time::sleep(timing.poll).await;
        waited = true;
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Finalization {
    InsertOnce,
    FinishSession,
    CopyOnly,
}

pub(crate) struct LiveTask {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<LiveDraft>,
}

impl LiveTask {
    /// Asks the live loop to stop without waiting for it. The loop checks this
    /// before each snapshot and decode, so a stop signalled before audio
    /// finalization starts no new live decode; one already running drains.
    pub(crate) fn signal_stop(&self) {
        self.stop.send_replace(true);
    }

    /// Stops the live loop if it is still running and joins its owner, which
    /// returns the draft for finalization.
    pub(crate) async fn finish(self) -> Result<LiveDraft, String> {
        self.signal_stop();
        self.task
            .await
            .map_err(|_| "live dictation task failed".to_string())
    }
}

/// Owns the provisional range until finalization or cancellation, including
/// every early-return path in the stop command.
pub(crate) struct LiveDraft {
    pub(crate) injector: SystemTextInjector,
    pub(crate) monitor: Arc<InputMonitor>,
    target: TargetWindow,
    session: Option<ProvisionalInsertion>,
    checkpoint: Option<u64>,
    /// The element focused when recording started, if UI Automation could
    /// identify it. `None` falls back to the window-level target guard.
    focus_identity: Option<Vec<i32>>,
    /// Input arrived after recording started and the checkpoint was renewed.
    /// Only then does the final paste require an empty selection and an
    /// explicitly inactive IME; without input the recording-start state holds.
    after_input: bool,
    attempted: bool,
    defer_insertion: bool,
    pub(crate) pasted: bool,
    /// How this recording's live-preview requests decode, fixed from the
    /// settings the recording started with.
    preview_decoding: asr::PreviewDecoding,
    /// Incremental final recognition when its setting was on at start; the
    /// stop path takes it for the final recognition.
    pub(crate) incremental: Option<incremental::Take>,
}

impl LiveDraft {
    fn can_insert_final(&self, text: &str) -> bool {
        !text.is_empty()
            && self
                .checkpoint
                .is_some_and(|checkpoint| self.monitor.unchanged_since(checkpoint))
    }

    /// The field focused at recording start is focused again. In single-window
    /// apps a click can move focus to another field (another chat, a terminal)
    /// without changing the window, so the window guard alone cannot tell.
    fn focus_unchanged(&self) -> bool {
        self.focus_identity.as_ref().is_none_or(|identity| {
            self.injector.focus_identity(&self.target).as_ref() == Some(identity)
        })
    }

    pub(crate) fn new(
        target: TargetWindow,
        settings: &Settings,
        active_hotkey: &str,
        mode_shortcuts: &[String],
        from_shortcut: bool,
        defer_insertion: bool,
    ) -> Self {
        let monitor = Arc::new(InputMonitor::default());
        let checkpoint = monitor
            .start_for_recording(active_hotkey, mode_shortcuts, from_shortcut)
            .then(|| monitor.checkpoint())
            .flatten();
        let injector = SystemTextInjector::new(InjectionOptions {
            restore_clipboard: settings.clipboard_restore,
        });
        let focus_identity = injector.focus_identity(&target);
        Self {
            injector,
            monitor,
            target,
            session: None,
            checkpoint,
            focus_identity,
            after_input: false,
            attempted: false,
            defer_insertion,
            pasted: false,
            preview_decoding: preview_decoding(settings),
            incremental: incremental::Take::plan(settings),
        }
    }

    /// Whether `update` can still edit the target. Only then must a caller
    /// let a held recording shortcut go first; a deferred final paste waits
    /// for released keys while settling and is guarded against them.
    pub(crate) fn edits_target_live(&self) -> bool {
        !self.defer_insertion || self.session.is_some()
    }

    /// Shows a new live hypothesis. An empty one is ignored; withdrawing text
    /// already shown is `retract`'s job.
    pub(crate) fn update(&mut self, text: &str) -> Result<(), injection::InjectionError> {
        if text.is_empty() || self.monitor.shortcut_pending() || self.defer_insertion {
            return Ok(());
        }
        if let Some(session) = self.session.as_mut() {
            let result = self
                .injector
                .update_provisional(session, text, &self.monitor)?;
            confirmed(result)?;
        } else if !self.attempted {
            self.attempted = true;
            // An interaction before the first result must also block insertion.
            if self
                .checkpoint
                .is_some_and(|c| self.monitor.unchanged_since(c))
            {
                self.session = self.injector.begin_live_provisional(
                    text,
                    &self.target,
                    &self.monitor,
                    self.checkpoint.expect("checkpoint was verified"),
                )?;
            }
            self.pasted |= self.session.as_ref().is_some_and(|s| s.paste_was_queued());
            if !self.pasted {
                // Live text requires a readable range; a final additive paste
                // is still allowed if no input was queued and guards hold.
                self.session.take();
            }
            if !self.pasted {
                return Err(injection::InjectionError::BackendFailure(
                    "live insertion could not be verified",
                ));
            }
        }
        self.pasted |= self.session.as_ref().is_some_and(|s| s.paste_was_queued());
        Ok(())
    }

    /// Withdraws the uncommitted part of the live draft back to `committed`,
    /// which may be empty, after an utterance ended without text. It only
    /// edits the draft this session inserted, under the same guards as a
    /// revision, and never starts an insertion of its own.
    pub(crate) fn retract(&mut self, committed: &str) -> Result<(), injection::InjectionError> {
        if self.monitor.shortcut_pending() || self.defer_insertion {
            return Ok(());
        }
        let Some(session) = self.session.as_mut() else {
            return Ok(());
        };
        let result = if committed.is_empty() {
            self.injector.retract_provisional(session, &self.monitor)?
        } else {
            self.injector
                .update_provisional(session, committed, &self.monitor)?
        };
        confirmed(result)
    }

    /// Input during recording and recognition does not block a deferred final
    /// paste: once input is quiet and the target is ready, the checkpoint is
    /// renewed, so only input during the paste itself stops it. Live sessions
    /// keep the recording-start checkpoint their replacement ranges rely on.
    /// Returns whether it had to wait for input to settle.
    pub(crate) async fn settle_before_final(&mut self, cancel: &watch::Receiver<bool>) -> bool {
        self.settle_before_final_with_hint(cancel, || {}).await
    }

    /// `settle_before_final`, calling `on_unready` once if the original
    /// field stays unfocused or its IME keeps composing for a while. The
    /// wait limit and the outcome are the same.
    pub(crate) async fn settle_before_final_with_hint(
        &mut self,
        cancel: &watch::Receiver<bool>,
        on_unready: impl FnOnce(),
    ) -> bool {
        // Without an available input monitor there is nothing to wait for.
        if !self.defer_insertion
            || self.pasted
            || self.session.is_some()
            || self.monitor.checkpoint().is_none()
        {
            return false;
        }
        let ready =
            |draft: &Self| draft.injector.target_ready(&draft.target) && draft.focus_unchanged();
        if self
            .checkpoint
            .is_some_and(|checkpoint| self.monitor.unchanged_since(checkpoint))
            && ready(self)
        {
            return false;
        }
        let draft = &*self;
        let settled = settled_checkpoint(
            &draft.monitor,
            cancel,
            SETTLE_TIMING,
            || ready(draft),
            on_unready,
        )
        .await;
        if let Some(checkpoint) = settled.checkpoint {
            self.renew_checkpoint(checkpoint);
        }
        // Input that was already quiet with a ready target needed no wait.
        settled.waited
    }

    fn renew_checkpoint(&mut self, checkpoint: u64) {
        // A changed input sequence means the user acted after recording began.
        self.after_input = self.checkpoint != Some(checkpoint);
        self.checkpoint = Some(checkpoint);
    }

    /// A live session finishes its own range. Without one, the final text is
    /// pasted at most once, never after a live paste was already queued, and
    /// only into the field that was focused when recording started.
    fn finalization(&self, text: &str) -> Finalization {
        if self.session.is_some() {
            Finalization::FinishSession
        } else if !self.pasted && self.can_insert_final(text) && self.focus_unchanged() {
            Finalization::InsertOnce
        } else {
            Finalization::CopyOnly
        }
    }

    pub(crate) fn finish(
        &mut self,
        text: &str,
        cancel: &watch::Receiver<bool>,
    ) -> Result<InsertResult, injection::InjectionError> {
        match self.finalization(text) {
            Finalization::InsertOnce => {
                self.attempted = true;
                let result = self.injector.insert_final(
                    text,
                    &self.target,
                    &self.monitor,
                    self.checkpoint.expect("checkpoint was verified"),
                    cancel,
                    self.after_input,
                )?;
                self.pasted |= result != InsertResult::ClipboardOnly;
                Ok(result)
            }
            Finalization::FinishSession => {
                // On cancel, dropping the draft removes its unedited range.
                if *cancel.borrow() {
                    return Err(injection::InjectionError::Cancelled);
                }
                let session = self.session.as_mut().expect("session was checked");
                self.injector
                    .finish_provisional(session, text, &self.monitor)
            }
            Finalization::CopyOnly => {
                // A cancelled monitor also fails `can_insert_final`; that is a
                // cancellation, not a reason to overwrite the clipboard.
                if *cancel.borrow() {
                    return Err(injection::InjectionError::Cancelled);
                }
                self.injector
                    .copy_to_clipboard(text)
                    .map(|()| InsertResult::ClipboardOnly)
            }
        }
    }

    /// Reason for a non-confirmed outcome. Call before dropping the draft,
    /// which stops the input monitor.
    pub(crate) fn diagnose(&self, result: InsertResult) -> Option<InsertionDetail> {
        // A paste that was never sent because focus moved to another field.
        if result == InsertResult::ClipboardOnly && !self.focus_unchanged() {
            return Some(InsertionDetail::TargetChanged);
        }
        (result != InsertResult::ClipboardPaste).then(|| {
            self.injector.diagnose_insertion(
                &self.target,
                &self.monitor,
                self.checkpoint,
                result == InsertResult::PasteUnverified,
                self.after_input,
            )
        })
    }
}

/// A live edit of an existing draft must be confirmed; otherwise live
/// insertion pauses and the final text is only copied.
fn confirmed(result: InsertResult) -> Result<(), injection::InjectionError> {
    if result == InsertResult::ClipboardPaste {
        Ok(())
    } else {
        Err(injection::InjectionError::BackendFailure(
            "live insertion could not be confirmed",
        ))
    }
}

impl Drop for LiveDraft {
    fn drop(&mut self) {
        // No updater retains this session. Cancellation cleanup may now remove
        // its exact unedited range using the ordinary interaction guards.
        self.monitor.observe_cancellation(None);
        if let Some(session) = self.session.as_mut() {
            self.injector.cancel_provisional(session, &self.monitor);
        }
        self.monitor.shutdown();
    }
}

pub(crate) fn start(
    app: AppHandle,
    operation_id: u64,
    mut draft: LiveDraft,
    prompt: Option<String>,
    speech_locale: Option<String>,
    cancel: watch::Receiver<bool>,
    preview_enabled: bool,
) -> LiveTask {
    draft.monitor.observe_cancellation(Some(cancel.clone()));
    if preview_enabled {
        // Called while recording starts, after its phase was published, so a
        // later stop or cancel always ends the subtitle room again.
        recording_overlay::enable_live_subtitles(&app);
    }
    let (stop, stopped) = watch::channel(false);
    let decoding = draft.preview_decoding;
    let task = spawn_recording_owner(async move {
        let mut incremental = draft
            .incremental
            .take()
            .map(|take| take.begin(prompt.as_deref(), speech_locale.as_deref()));
        if !preview_enabled
            && incremental
                .as_mut()
                .and_then(incremental::Take::session_mut)
                .is_none()
        {
            draft.incremental = incremental;
            return draft;
        }
        let services = app.state::<Services>();
        let result = run_loop(
            &services.audio,
            services.transcriber.as_ref(),
            prompt.as_deref(),
            speech_locale.as_deref(),
            decoding,
            cancel,
            stopped,
            LIVE_INTERVAL,
            Work {
                preview: preview_enabled,
                incremental: incremental.as_mut().and_then(incremental::Take::session_mut),
            },
            |live| {
                if services.lifecycle.phase() != PipelinePhase::Recording
                    || services.lifecycle.is_cancelled(operation_id)
                {
                    return;
                }
                let state = app.state::<AppState>();
                let _ = app.emit("app-state", state.publish_result(live.text().to_owned()));
                // A retraction updates the subtitle like a revision does.
                recording_overlay::show_live_subtitle(&app, live.text());
                let result = match live {
                    LiveText::Revise(text) => draft.update(text),
                    LiveText::Retract(committed) => draft.retract(committed),
                };
                if result.is_err() {
                    emit_status(&app, "live_insertion_unavailable", "Live text insertion paused. The final result will remain available in this app.");
                }
            },
        )
        .await;
        draft.incremental = incremental;
        if let Err(failure) = result {
            if !services.lifecycle.is_cancelled(operation_id) {
                let (kind, message) = failure.status();
                emit_status(&app, kind, message);
            }
        }
        draft
    });
    LiveTask { stop, task }
}

/// One blocking worker owns a recording's serial live loop. UIA, clipboard
/// waits, temporary-file cleanup and draft teardown stay off runtime workers;
/// asynchronous ASR and timers still use the originating runtime. Stop joins
/// this owner before finalization, so no queued update outlives its draft.
pub(crate) fn spawn_recording_owner<F>(work: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || runtime.block_on(work))
}

/// Why live recognition ended early. Only a recognition failure leaves the
/// microphone recording, so only it may promise a full transcription on stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveFailure {
    /// The microphone stream itself failed (for example a disconnected headset).
    Capture,
    /// Preparing or recognizing captured audio failed.
    Recognition,
}

impl LiveFailure {
    fn status(self) -> (&'static str, &'static str) {
        match self {
            Self::Capture => (
                "live_capture_failed",
                "Live transcription stopped because the microphone input failed.",
            ),
            Self::Recognition => (
                "live_transcription_failed",
                "Live transcription failed. The full recording will be transcribed after stopping.",
            ),
        }
    }
}

impl From<String> for LiveFailure {
    fn from(_: String) -> Self {
        Self::Recognition
    }
}

impl From<&str> for LiveFailure {
    fn from(_: &str) -> Self {
        Self::Recognition
    }
}

/// Serial snapshot inference provides backpressure: slow models never build a
/// queue. Stop drains the current request; cancellation returns at once
/// while the transcriber cancels the request in the worker, which keeps
/// the loaded model for the next recording. A late result is never
/// published.
///
/// The live preview alone, as the replay tests drive it.
#[cfg(test)]
#[allow(
    clippy::too_many_arguments,
    reason = "The replay seam keeps independent stop/cancel channels and borrowed recognition inputs explicit."
)]
async fn run(
    audio: &tokio::sync::Mutex<Box<dyn AudioCapture>>,
    transcriber: &dyn Transcriber,
    prompt: Option<&str>,
    speech_locale: Option<&str>,
    decoding: asr::PreviewDecoding,
    cancel: watch::Receiver<bool>,
    stopped: watch::Receiver<bool>,
    interval: Duration,
    on_update: impl FnMut(&LiveText),
) -> Result<(), LiveFailure> {
    run_loop(
        audio,
        transcriber,
        prompt,
        speech_locale,
        decoding,
        cancel,
        stopped,
        interval,
        Work {
            preview: true,
            incremental: None,
        },
        on_update,
    )
    .await
}

/// What the serial live loop does each turn.
struct Work<'a> {
    /// Recognize and show live preview windows.
    preview: bool,
    /// Recognize incremental final ranges (see [`incremental`]). A turn that
    /// decodes a range skips the preview; the next turn goes to the preview.
    incremental: Option<&'a mut incremental::Session>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "The replay seam keeps independent stop/cancel channels and borrowed recognition inputs explicit."
)]
async fn run_loop(
    audio: &tokio::sync::Mutex<Box<dyn AudioCapture>>,
    transcriber: &dyn Transcriber,
    prompt: Option<&str>,
    speech_locale: Option<&str>,
    decoding: asr::PreviewDecoding,
    mut cancel: watch::Receiver<bool>,
    mut stopped: watch::Receiver<bool>,
    interval: Duration,
    mut work: Work<'_>,
    mut on_update: impl FnMut(&LiveText),
) -> Result<(), LiveFailure> {
    let mut segmentation = Segmentation::default();
    let mut gate = PreparationGate::default();
    let mut cursor = Duration::ZERO;
    let mut completed = String::new();
    let mut displayed = String::new();
    loop {
        if *cancel.borrow() || *stopped.borrow() {
            return Ok(());
        }
        tokio::select! {
            _ = cancel.changed() => return Ok(()),
            _ = stopped.changed() => return Ok(()),
            _ = tokio::time::sleep(interval) => {},
        }
        if let Some(session) = work.incremental.as_mut() {
            match session
                .step(audio, transcriber, &cancel, &stopped, work.preview)
                .await
            {
                // Without a preview, a session that gave up leaves nothing to do.
                incremental::Turn::Idle if !work.preview && !session.active() => return Ok(()),
                incremental::Turn::Idle => {}
                incremental::Turn::Used => continue,
                incremental::Turn::Stop => return Ok(()),
                incremental::Turn::CaptureFailed => return Err(LiveFailure::Capture),
            }
        }
        if !work.preview {
            continue;
        }
        let snapshot = {
            let audio = audio.lock().await;
            if *cancel.borrow() || *stopped.borrow() {
                return Ok(());
            }
            if let Some(unseen) = gate.unseen_from() {
                match audio.probe(cursor, cursor + unseen) {
                    Ok(Some(probe)) if !gate.should_prepare(&probe, &segmentation) => continue,
                    Ok(_) => {}
                    Err(AudioError::NotCapturing) => return Ok(()),
                    Err(_) => return Err(LiveFailure::Capture),
                }
            }
            match audio.snapshot(cursor) {
                Ok(snapshot) => snapshot,
                Err(AudioError::NotCapturing) => return Ok(()),
                Err(_) => return Err(LiveFailure::Capture),
            }
        };
        let window = snapshot.duration();
        // Every outcome but `Wait` moves the cursor or the segmentation state.
        gate.reset();
        let mut prepared = match tokio::task::spawn_blocking(move || snapshot.prepare())
            .await
            .map_err(|_| "live audio preparation failed")?
        {
            Ok(prepared) => prepared,
            Err(AudioError::TooShort { .. } | AudioError::NoVoiceDetected) => continue,
            Err(_) => return Err(LiveFailure::Recognition),
        };
        let (samples, endpoint, voiced_until) = match segmentation.plan(&prepared) {
            Decision::Skip(silence) => {
                cursor += silence;
                continue;
            }
            Decision::Wait => {
                gate.waited(window, trailing_quiet(&prepared));
                continue;
            }
            Decision::Decode {
                samples,
                endpoint,
                voiced_until,
            } => (samples, endpoint, voiced_until),
        };
        // After a partial decode the same audio would plan `Wait`, so the
        // gate can hold off until new audio could change that.
        let partial_quiet = (!endpoint).then(|| trailing_quiet(&prepared));
        prepared.samples.truncate(samples);
        let decoded_duration = prepared.duration();
        let artifact = tokio::task::spawn_blocking(move || prepared.into_artifact())
            .await
            .map_err(|_| "live audio writing failed")?;
        // A silent window is never decoded but still ends its utterance, so it
        // reconciles the display like an empty recognition result.
        let (utterance, inference_cost) = match window_artifact(artifact)? {
            None => (None, Duration::ZERO),
            Some(artifact) => {
                let mut cleanup = TempArtifact::new(artifact.path.clone(), false);
                if *cancel.borrow() || *stopped.borrow() {
                    return Ok(());
                }
                let inference_started = Instant::now();
                let result = transcriber
                    .transcribe_preview(
                        &artifact.path,
                        prompt,
                        speech_locale,
                        decoding,
                        cancel.clone(),
                    )
                    .await;
                let inference_cost = inference_started.elapsed();
                cleanup.cleanup().map_err(command_error)?;
                if *cancel.borrow() || *stopped.borrow() {
                    return Ok(());
                }
                (window_text(result)?, inference_cost)
            }
        };
        if let Some(live) = window_display(&completed, utterance.as_deref(), endpoint) {
            if live.text() != displayed {
                on_update(&live);
                displayed = live.text().to_owned();
            }
            if endpoint {
                completed = live.text().to_owned();
            }
        }
        // Advance even past a skipped window; re-planning it would decode it forever.
        segmentation.recognized(decoded_duration, voiced_until, inference_cost, endpoint);
        if endpoint {
            cursor += decoded_duration;
        }
        if let Some(quiet) = partial_quiet {
            gate.waited(window, quiet);
        }
    }
}

/// A window with nothing recognizable in it, such as a cough or breath that
/// passed the segmentation energy gate, is skipped (`None`) so live
/// recognition keeps running. Only real audio failures end live mode.
fn window_artifact(
    result: Result<audio::AudioArtifact, AudioError>,
) -> Result<Option<audio::AudioArtifact>, String> {
    match result {
        Ok(artifact) => Ok(Some(artifact)),
        Err(AudioError::TooShort { .. } | AudioError::NoVoiceDetected) => Ok(None),
        Err(error) => Err(command_error(error)),
    }
}

/// What one live window asks the preview and the target draft to show.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LiveText {
    /// A new, non-empty hypothesis for the live text.
    Revise(String),
    /// The utterance ended without text: withdraw its uncommitted partial
    /// back to the committed text, which may be empty.
    Retract(String),
}

impl LiveText {
    fn text(&self) -> &str {
        match self {
            Self::Revise(text) | Self::Retract(text) => text,
        }
    }
}

/// The live text after one window, or `None` to keep the current display. An
/// utterance endpoint without text (an empty result or a silent window)
/// withdraws the partial shown earlier for that utterance, since it will
/// never be committed. An empty window mid-utterance changes nothing.
fn window_display(completed: &str, utterance: Option<&str>, endpoint: bool) -> Option<LiveText> {
    match utterance {
        Some(utterance) => Some(LiveText::Revise(append_utterance(completed, utterance))),
        None if endpoint => Some(LiveText::Retract(completed.to_owned())),
        None => None,
    }
}

/// An empty recognition result skips the window (`None`); worker and
/// transport errors still end live mode.
fn window_text(result: Result<asr::Transcript, asr::AsrError>) -> Result<Option<String>, String> {
    let transcript = result.map_err(command_error)?;
    Ok((!transcript.text.trim().is_empty()).then_some(transcript.text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::WorkerCommand;
    use audio::{
        AudioArtifact, AudioDevice, AudioFuture, AudioSnapshot, CaptureConfig, CaptureState,
        LevelMeter,
    };
    use injection::test_backend::{self, MockBackend};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::{
        fs,
        path::{Path, PathBuf},
    };
    use tokio::sync::Notify;

    fn deferred_test_draft() -> LiveDraft {
        let monitor = Arc::new(InputMonitor::default());
        monitor.test_set_available(true);
        LiveDraft {
            injector: SystemTextInjector::new(InjectionOptions::default()),
            monitor,
            target: TargetWindow {
                window_handle: 0,
                control_handle: 0,
                process_id: 0,
                thread_id: 0,
                is_secure: false,
            },
            session: None,
            checkpoint: Some(0),
            focus_identity: None,
            after_input: false,
            attempted: false,
            defer_insertion: true,
            pasted: false,
            preview_decoding: asr::PreviewDecoding::Standard,
            incremental: None,
        }
    }

    #[test]
    fn dictate_defers_target_input_unless_live_insertion_is_enabled() {
        let mut settings = Settings::default();
        assert!(defers_target_insertion(PipelineMode::Dictate, &settings));
        assert!(defers_target_insertion(PipelineMode::Translate, &settings));
        settings.live_target_insertion = true;
        assert!(!defers_target_insertion(PipelineMode::Dictate, &settings));
        settings.automatic_voice_mode = true;
        assert!(defers_target_insertion(PipelineMode::Dictate, &settings));
        for mode in [
            PipelineMode::Translate,
            PipelineMode::Edit,
            PipelineMode::Ask,
        ] {
            assert!(defers_target_insertion(mode, &settings));
        }
    }

    /// A microphone whose stream has failed, as after a headset disconnect.
    struct FailedCapture;
    impl AudioCapture for FailedCapture {
        fn snapshot(&self, _: Duration) -> Result<AudioSnapshot, AudioError> {
            Err(AudioError::StreamFailure("device disconnected".into()))
        }
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
        fn stop(&mut self) -> AudioFuture<'_, AudioArtifact> {
            panic!("live inference must not stop capture")
        }
        fn cancel(&mut self) -> AudioFuture<'_, ()> {
            panic!("live inference must not cancel capture")
        }
        fn state(&self) -> CaptureState {
            CaptureState::Capturing
        }
        fn level(&self) -> LevelMeter {
            LevelMeter::default()
        }
    }

    #[tokio::test]
    async fn a_failed_microphone_is_reported_as_a_capture_failure() {
        let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> =
            tokio::sync::Mutex::new(Box::new(FailedCapture));
        let recognizer = Recognizer {
            calls: AtomicUsize::new(0),
            entered: Notify::new(),
            release: None,
        };
        let (_cancel, cancel) = watch::channel(false);
        let (_stop, stopped) = watch::channel(false);
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            run(
                &audio,
                &recognizer,
                None,
                None,
                asr::PreviewDecoding::Standard,
                cancel,
                stopped,
                Duration::from_millis(1),
                |_| panic!("no text without audio"),
            ),
        )
        .await
        .unwrap();
        assert_eq!(result, Err(LiveFailure::Capture));
        assert_eq!(recognizer.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn only_a_recognition_failure_promises_a_full_transcription() {
        let (capture_kind, capture) = LiveFailure::Capture.status();
        let (recognition_kind, recognition) = LiveFailure::Recognition.status();
        assert_ne!(capture_kind, recognition_kind);
        assert!(!capture.contains("full recording"));
        assert!(recognition.contains("full recording will be transcribed"));
        assert_eq!(
            LiveFailure::from(String::from("worker failed")),
            LiveFailure::Recognition
        );
    }

    // Windows timers can round short sleeps up to ~16 ms; the quiet window
    // stays several input periods long so these tests are not timing-flaky.
    const FAST: SettleTiming = SettleTiming {
        quiet: Duration::from_millis(80),
        limit: Duration::from_millis(300),
        poll: Duration::from_millis(5),
        hint: Duration::from_millis(150),
    };

    fn available_monitor() -> Arc<InputMonitor> {
        let monitor = Arc::new(InputMonitor::default());
        monitor.test_set_available(true);
        monitor
    }

    fn ago(duration: Duration) -> Instant {
        Instant::now()
            .checked_sub(duration)
            .expect("the clock is past the test offset")
    }

    #[tokio::test]
    async fn input_that_ended_before_settling_adds_no_quiet_wait() {
        let monitor = available_monitor();
        monitor.test_record_input_at(ago(Duration::from_secs(1)));
        let (_cancel, cancelled) = watch::channel(false);
        let started = Instant::now();
        let settled = settled_checkpoint(&monitor, &cancelled, FAST, || true, || {}).await;
        // Settling on the first check is not reported as a wait.
        assert_eq!(
            settled,
            Settled {
                checkpoint: Some(1),
                waited: false
            }
        );
        assert!(started.elapsed() < FAST.quiet, "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn quiet_time_counts_from_the_last_input_not_from_the_wait() {
        let timing = SettleTiming {
            quiet: Duration::from_millis(200),
            limit: Duration::from_secs(2),
            ..FAST
        };
        let monitor = available_monitor();
        let input = ago(Duration::from_millis(150));
        monitor.test_record_input_at(input);
        let (_cancel, cancelled) = watch::channel(false);
        let started = Instant::now();
        let settled = settled_checkpoint(&monitor, &cancelled, timing, || true, || {}).await;
        assert_eq!(
            settled,
            Settled {
                checkpoint: Some(1),
                waited: true
            }
        );
        // A full quiet window after the input, but not one after the start.
        assert!(input.elapsed() >= timing.quiet);
        assert!(started.elapsed() < timing.quiet, "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_held_shortcut_is_never_quiet_even_after_old_input() {
        let monitor = available_monitor();
        monitor.test_record_input_at(ago(Duration::from_secs(1)));
        monitor.test_set_async_modifier_pending(true);
        let (_cancel, cancelled) = watch::channel(false);
        assert_eq!(
            settled_checkpoint(&monitor, &cancelled, FAST, || true, || {})
                .await
                .checkpoint,
            None
        );
    }

    #[tokio::test]
    async fn an_unready_target_is_reported_once_it_has_waited_for_the_hint_delay() {
        let timing = SettleTiming {
            limit: Duration::from_millis(600),
            ..FAST
        };
        let monitor = available_monitor();
        let (_cancel, cancelled) = watch::channel(false);
        let started = Instant::now();
        let reported = std::cell::Cell::new(None);
        let checkpoint = settled_checkpoint(
            &monitor,
            &cancelled,
            timing,
            || false,
            || reported.set(Some(started.elapsed())),
        )
        .await
        .checkpoint;
        // The safety outcome is unchanged: a quiet checkpoint at the limit.
        assert_eq!(checkpoint, Some(0));
        let reported = reported.get().expect("the wait was reported");
        assert!(reported >= timing.hint, "{reported:?}");
        assert!(reported < timing.limit, "{reported:?}");
    }

    #[tokio::test]
    async fn a_target_that_becomes_ready_before_the_hint_delay_is_not_reported() {
        let monitor = available_monitor();
        let (_cancel, cancelled) = watch::channel(false);
        let started = Instant::now();
        let reported = std::cell::Cell::new(false);
        let checkpoint = settled_checkpoint(
            &monitor,
            &cancelled,
            FAST,
            || started.elapsed() >= Duration::from_millis(40),
            || reported.set(true),
        )
        .await
        .checkpoint;
        assert_eq!(checkpoint, Some(0));
        assert!(!reported.get());
    }

    #[test]
    fn only_a_draft_that_edits_the_target_live_waits_for_the_shortcut_first() {
        let mut draft = deferred_test_draft();
        assert!(!draft.edits_target_live());
        draft.defer_insertion = false;
        assert!(draft.edits_target_live());
    }

    #[tokio::test]
    async fn clicks_during_recording_settle_to_a_fresh_checkpoint() {
        let mut draft = deferred_test_draft();
        draft.monitor.test_record_input();
        assert!(!draft.can_insert_final("final"));
        let (_cancel, cancelled) = watch::channel(false);
        let checkpoint = settled_checkpoint(&draft.monitor, &cancelled, FAST, || true, || {})
            .await
            .checkpoint
            .unwrap();
        draft.checkpoint = Some(checkpoint);
        assert!(draft.can_insert_final("final"));
        // Input during the paste itself still stops it.
        draft.monitor.test_record_input();
        assert!(!draft.can_insert_final("final"));
    }

    #[tokio::test]
    async fn continuous_input_never_settles() {
        let monitor = available_monitor();
        let busy = Arc::clone(&monitor);
        let done = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&done);
        // The user is already typing when settling starts.
        monitor.test_record_input();
        // Always runnable, so it records input between any two polls on this
        // single-threaded runtime, independent of timer resolution or stalls.
        let typing = tokio::spawn(async move {
            while !stop.load(Ordering::Acquire) {
                busy.test_record_input();
                tokio::task::yield_now().await;
            }
        });
        let (_cancel, cancelled) = watch::channel(false);
        let result = settled_checkpoint(&monitor, &cancelled, FAST, || true, || {})
            .await
            .checkpoint;
        done.store(true, Ordering::Release);
        typing.await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn settling_without_an_input_monitor_reports_no_wait() {
        let (_cancel, cancelled) = watch::channel(false);
        let mut draft = deferred_test_draft();
        draft.monitor.test_set_available(false);
        let waited = tokio::time::timeout(
            Duration::from_millis(50),
            draft.settle_before_final(&cancelled),
        )
        .await
        .unwrap();
        assert!(!waited);
        assert_eq!(draft.checkpoint, Some(0));
    }

    #[tokio::test]
    async fn settling_leaves_live_sessions_and_queued_pastes_alone() {
        let (_cancel, cancelled) = watch::channel(false);
        for case in ["live", "pasted"] {
            let mut draft = deferred_test_draft();
            draft.monitor.test_record_input();
            match case {
                "live" => draft.defer_insertion = false,
                "pasted" => draft.pasted = true,
                _ => unreachable!(),
            }
            let waited = tokio::time::timeout(
                Duration::from_millis(50),
                draft.settle_before_final(&cancelled),
            )
            .await
            .unwrap();
            // Skipping the settle is not reported as a wait.
            assert!(!waited, "{case}");
            // The recording-start checkpoint is kept, so input still blocks.
            assert_eq!(draft.checkpoint, Some(0), "{case}");
            assert!(!draft.can_insert_final("final"), "{case}");
        }
    }

    #[test]
    fn only_a_changed_input_sequence_makes_the_final_paste_strict() {
        let mut draft = deferred_test_draft();
        // Waited for focus or IME without any input: recording-start rules.
        draft.renew_checkpoint(0);
        assert!(!draft.after_input);
        // Clicked or scrolled while waiting: strict selection and IME rules.
        draft.renew_checkpoint(3);
        assert!(draft.after_input);
        assert_eq!(draft.checkpoint, Some(3));
    }

    #[test]
    fn a_different_focused_field_is_never_pasted_into() {
        let mut draft = deferred_test_draft();
        assert_eq!(draft.finalization("final"), Finalization::InsertOnce);
        // The test target belongs to no process, so the element focused now
        // can never match the identity recorded at recording start.
        draft.focus_identity = Some(vec![42, 7]);
        assert!(!draft.focus_unchanged());
        assert_eq!(draft.finalization("final"), Finalization::CopyOnly);
        assert_eq!(
            draft.diagnose(InsertResult::ClipboardOnly),
            Some(InsertionDetail::TargetChanged)
        );
    }

    #[tokio::test]
    async fn a_quiet_but_unready_target_yields_a_checkpoint_at_the_limit() {
        let monitor = available_monitor();
        monitor.test_record_input();
        let (_cancel, cancelled) = watch::channel(false);
        let started = Instant::now();
        let checkpoint = settled_checkpoint(&monitor, &cancelled, FAST, || false, || {})
            .await
            .checkpoint;
        assert_eq!(checkpoint, monitor.checkpoint());
        assert!(started.elapsed() >= FAST.limit);
    }

    #[tokio::test]
    async fn cancellation_or_a_stopped_monitor_ends_the_wait_without_a_checkpoint() {
        let monitor = available_monitor();
        let (cancel, cancelled) = watch::channel(false);
        cancel.send_replace(true);
        assert_eq!(
            settled_checkpoint(&monitor, &cancelled, FAST, || true, || {})
                .await
                .checkpoint,
            None
        );
        let (_cancel, cancelled) = watch::channel(false);
        monitor.test_set_available(false);
        assert_eq!(
            settled_checkpoint(&monitor, &cancelled, FAST, || true, || {})
                .await
                .checkpoint,
            None
        );
    }

    #[test]
    fn final_text_is_pasted_at_most_once_and_never_after_a_queued_live_paste() {
        let mut draft = deferred_test_draft();
        assert_eq!(draft.finalization("final"), Finalization::InsertOnce);
        assert_eq!(draft.finalization(""), Finalization::CopyOnly);
        // A queued live paste (its session already taken or finished) must
        // never be followed by a second additive paste of the final text.
        draft.pasted = true;
        assert_eq!(draft.finalization("final"), Finalization::CopyOnly);
        let draft = deferred_test_draft();
        draft.monitor.test_record_input();
        assert_eq!(draft.finalization("final"), Finalization::CopyOnly);
    }

    #[test]
    fn cancelled_finalization_neither_inserts_nor_copies() {
        let mut draft = deferred_test_draft();
        let (cancel, cancelled) = watch::channel(false);
        draft.monitor.observe_cancellation(Some(cancelled.clone()));
        cancel.send_replace(true);
        assert_eq!(draft.finalization("final"), Finalization::CopyOnly);
        assert_eq!(
            draft.finish("final", &cancelled),
            Err(injection::InjectionError::Cancelled)
        );
        assert!(!draft.pasted);
    }

    #[test]
    fn translate_draft_defers_raw_text_and_keeps_original_checkpoint() {
        let mut draft = deferred_test_draft();
        draft.update("raw speech").unwrap();
        assert!(!draft.attempted);
        assert!(draft.session.is_none());
        assert!(!draft.pasted);
        assert!(draft.can_insert_final("translated speech"));
        draft.monitor.test_record_input();
        assert!(!draft.can_insert_final("translated speech"));
    }

    #[test]
    fn retraction_never_starts_an_insertion() {
        for deferred in [true, false] {
            let mut draft = deferred_test_draft();
            draft.defer_insertion = deferred;
            // Without a live draft in the target there is nothing to withdraw.
            draft.retract("").unwrap();
            draft.retract("前文").unwrap();
            // An ordinary empty update is still ignored.
            draft.update("").unwrap();
            assert!(!draft.attempted);
            assert!(draft.session.is_none());
            assert!(!draft.pasted);
        }
    }

    type SharedMock = Arc<std::sync::Mutex<MockBackend>>;

    /// A live-insertion draft whose injector drives the batch mock target,
    /// which reads "prefix | suffix" with the caret at `|`.
    fn live_test_draft() -> (LiveDraft, SharedMock) {
        let mock = Arc::new(std::sync::Mutex::new(MockBackend::new()));
        let monitor = available_monitor();
        let checkpoint = monitor.checkpoint();
        let draft = LiveDraft {
            injector: SystemTextInjector::with_mock(Arc::clone(&mock), InjectionOptions::default()),
            monitor,
            target: test_backend::target(),
            session: None,
            checkpoint,
            focus_identity: None,
            after_input: false,
            attempted: false,
            defer_insertion: false,
            pasted: false,
            preview_decoding: asr::PreviewDecoding::Standard,
            incremental: None,
        };
        (draft, mock)
    }

    fn content(mock: &SharedMock) -> String {
        mock.lock().unwrap().content()
    }

    fn mock_calls(mock: &SharedMock, name: &str) -> usize {
        test_backend::calls(&mock.lock().unwrap(), name)
    }

    #[test]
    fn an_empty_endpoint_deletes_the_live_draft_and_the_next_utterance_resumes_there() {
        let (mut draft, mock) = live_test_draft();
        draft.update("途中").unwrap();
        assert_eq!(content(&mock), "prefix 途中 suffix");
        assert!(draft.pasted);
        draft.retract("").unwrap();
        assert_eq!(content(&mock), "prefix  suffix");
        assert_eq!(mock_calls(&mock, "delete"), 1);
        // The session keeps the withdrawn range, and the queued live paste
        // still rules out a second additive paste of the final text.
        assert!(draft.session.is_some());
        assert!(draft.pasted);
        assert_eq!(draft.finalization("最終"), Finalization::FinishSession);
        draft.update("次の文").unwrap();
        assert_eq!(content(&mock), "prefix 次の文 suffix");
        let (_cancel, cancelled) = watch::channel(false);
        assert_eq!(
            draft.finish("次の文。", &cancelled),
            Ok(InsertResult::ClipboardPaste)
        );
        assert_eq!(content(&mock), "prefix 次の文。 suffix");
        drop(draft);
        // A finished session is never removed on drop.
        assert_eq!(content(&mock), "prefix 次の文。 suffix");
    }

    #[test]
    fn retracting_to_committed_text_replaces_only_the_uncommitted_partial() {
        let (mut draft, mock) = live_test_draft();
        draft.update("前文").unwrap();
        draft.update("前文途中").unwrap();
        assert_eq!(content(&mock), "prefix 前文途中 suffix");
        draft.retract("前文").unwrap();
        assert_eq!(content(&mock), "prefix 前文 suffix");
        // The guarded replacement selects the draft and pastes over it.
        assert_eq!(mock_calls(&mock, "delete"), 0);
        assert_eq!(mock_calls(&mock, "select"), 2);
        assert_eq!(mock_calls(&mock, "paste"), 3);
    }

    #[test]
    fn retraction_waits_for_a_pending_shortcut_and_never_touches_a_deferred_target() {
        for case in ["shortcut", "deferred"] {
            let (mut draft, mock) = live_test_draft();
            draft.update("途中").unwrap();
            let edits = || ["select", "paste", "delete"].map(|name| mock_calls(&mock, name));
            let before = edits();
            match case {
                "shortcut" => draft.monitor.test_set_async_modifier_pending(true),
                "deferred" => draft.defer_insertion = true,
                _ => unreachable!(),
            }
            draft.retract("").unwrap();
            draft.retract("前文").unwrap();
            assert_eq!(content(&mock), "prefix 途中 suffix", "{case}");
            assert_eq!(edits(), before, "{case}");
        }
    }

    #[test]
    fn only_a_confirmed_live_edit_keeps_live_insertion_running() {
        assert_eq!(confirmed(InsertResult::ClipboardPaste), Ok(()));
        for result in [InsertResult::PasteUnverified, InsertResult::ClipboardOnly] {
            assert!(confirmed(result).is_err());
        }
        let (mut draft, mock) = live_test_draft();
        draft.update("途中").unwrap();
        // An unknown IME state fails the deletion guard: live insertion
        // pauses and the target is never edited again.
        mock.lock().unwrap().ime.set(None);
        assert!(draft.retract("").is_err());
        mock.lock().unwrap().ime.set(Some(false));
        assert!(draft.update("次の文").is_err());
        let (_cancel, cancelled) = watch::channel(false);
        assert_eq!(
            draft.finish("最終", &cancelled),
            Ok(InsertResult::ClipboardOnly)
        );
        assert_eq!(content(&mock), "prefix 途中 suffix");
        assert_eq!(mock_calls(&mock, "delete"), 0);
    }

    #[test]
    fn translate_final_requires_available_monitor_and_uncancelled_checkpoint() {
        let draft = deferred_test_draft();
        assert!(!draft.can_insert_final(""));
        draft.monitor.test_set_available(false);
        assert!(!draft.can_insert_final("translated speech"));
        draft.monitor.test_set_available(true);
        let (cancel, cancelled) = watch::channel(false);
        draft.monitor.observe_cancellation(Some(cancelled));
        cancel.send_replace(true);
        assert!(!draft.can_insert_final("translated speech"));
    }

    /// Successive snapshots return the recording so far, the last stage
    /// repeating. Without stages, a tone grows after one second of silence.
    struct Capture {
        directory: PathBuf,
        snapshots: AtomicUsize,
        stages: Vec<Stage>,
    }

    struct Stage {
        samples: Vec<f32>,
        /// The shortest window this snapshot accepts for recognition.
        minimum_duration: Duration,
    }

    fn tone(seconds: f64) -> Vec<f32> {
        (0..(seconds * 24_000.0) as usize)
            .map(|i| (i as f32 * 0.1).sin() * 0.2)
            .collect()
    }

    fn stage(samples: Vec<f32>) -> Stage {
        Stage {
            samples,
            minimum_duration: CaptureConfig::default().minimum_duration,
        }
    }

    impl AudioCapture for Capture {
        fn snapshot(&self, since: Duration) -> Result<AudioSnapshot, AudioError> {
            let index = self.snapshots.fetch_add(1, Ordering::SeqCst);
            let (samples, minimum_duration) = match self
                .stages
                .get(index.min(self.stages.len().saturating_sub(1)))
            {
                Some(stage) => (stage.samples.clone(), stage.minimum_duration),
                None => {
                    let mut samples = vec![0.0; 24_000];
                    samples.extend(tone(index as f64));
                    (samples, CaptureConfig::default().minimum_duration)
                }
            };
            let offset = (since.as_secs_f64() * 24_000.0).round() as usize;
            let samples = samples[offset.min(samples.len())..].to_vec();
            Ok(AudioSnapshot::for_test(
                samples,
                CaptureConfig {
                    artifact_directory: Some(self.directory.clone()),
                    minimum_duration,
                    ..CaptureConfig::default()
                },
            ))
        }
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
        fn stop(&mut self) -> AudioFuture<'_, AudioArtifact> {
            panic!("live inference must not stop capture")
        }
        fn cancel(&mut self) -> AudioFuture<'_, ()> {
            panic!("live inference must not cancel capture")
        }
        fn state(&self) -> CaptureState {
            CaptureState::Capturing
        }
        fn level(&self) -> LevelMeter {
            LevelMeter::default()
        }
    }

    struct Recognizer {
        calls: AtomicUsize,
        entered: Notify,
        release: Option<Arc<Notify>>,
    }
    #[async_trait::async_trait]
    impl Transcriber for Recognizer {
        async fn load(&self, _: &str) -> Result<(), asr::AsrError> {
            Ok(())
        }
        async fn transcribe(
            &self,
            path: &Path,
            _: Option<&str>,
            _: watch::Receiver<bool>,
        ) -> Result<asr::Transcript, asr::AsrError> {
            assert!(path.is_file());
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            if let Some(release) = &self.release {
                release.notified().await;
            }
            Ok(asr::Transcript {
                text: if index == 0 {
                    "途中"
                } else {
                    "途中の文章"
                }
                .into(),
                segments: vec![],
                model: "test".into(),
                duration_ms: 1,
            })
        }
        async fn shutdown(&self) -> Result<(), asr::AsrError> {
            Ok(())
        }
        async fn reconfigure(&self, _: WorkerCommand) {}
    }

    #[tokio::test]
    async fn live_recognition_skips_silence_publishes_before_stop_and_cleans_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> =
            tokio::sync::Mutex::new(Box::new(Capture {
                directory: directory.path().to_owned(),
                snapshots: AtomicUsize::new(0),
                stages: vec![],
            }));
        let recognizer = Recognizer {
            calls: AtomicUsize::new(0),
            entered: Notify::new(),
            release: None,
        };
        let (_cancel, cancel) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let mut updates = vec![];
        tokio::time::timeout(
            Duration::from_secs(2),
            run(
                &audio,
                &recognizer,
                None,
                None,
                asr::PreviewDecoding::Standard,
                cancel,
                stopped,
                Duration::from_millis(1),
                |text| {
                    assert!(!*stop.borrow());
                    updates.push(text.to_owned());
                    if updates.len() == 2 {
                        stop.send_replace(true);
                    }
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updates, [revise("途中"), revise("途中の文章")]);
        assert_eq!(recognizer.calls.load(Ordering::SeqCst), 2);
        assert_eq!(audio.lock().await.state(), CaptureState::Capturing);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    /// Records how each live request asked to decode. Live recognition must
    /// never use the final-recognition request.
    struct DecodingRecognizer {
        seen: std::sync::Mutex<Vec<asr::PreviewDecoding>>,
    }
    #[async_trait::async_trait]
    impl Transcriber for DecodingRecognizer {
        async fn load(&self, _: &str) -> Result<(), asr::AsrError> {
            Ok(())
        }
        async fn transcribe(
            &self,
            _: &Path,
            _: Option<&str>,
            _: watch::Receiver<bool>,
        ) -> Result<asr::Transcript, asr::AsrError> {
            panic!("live recognition must send preview requests")
        }
        async fn transcribe_preview(
            &self,
            _: &Path,
            _: Option<&str>,
            _: Option<&str>,
            decoding: asr::PreviewDecoding,
            _: watch::Receiver<bool>,
        ) -> Result<asr::Transcript, asr::AsrError> {
            self.seen.lock().unwrap().push(decoding);
            Ok(asr::Transcript {
                text: "途中".into(),
                segments: vec![],
                model: "test".into(),
                duration_ms: 1,
            })
        }
        async fn shutdown(&self) -> Result<(), asr::AsrError> {
            Ok(())
        }
        async fn reconfigure(&self, _: WorkerCommand) {}
    }

    #[tokio::test]
    async fn live_requests_carry_the_recordings_preview_decoding() {
        let mut settings = Settings::default();
        assert_eq!(preview_decoding(&settings), asr::PreviewDecoding::Standard);
        settings.live_preview_fast_decode = true;
        assert_eq!(preview_decoding(&settings), asr::PreviewDecoding::Greedy);
        for decoding in [asr::PreviewDecoding::Standard, asr::PreviewDecoding::Greedy] {
            let directory = tempfile::tempdir().unwrap();
            let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> =
                tokio::sync::Mutex::new(Box::new(Capture {
                    directory: directory.path().to_owned(),
                    snapshots: AtomicUsize::new(0),
                    stages: vec![],
                }));
            let recognizer = DecodingRecognizer {
                seen: std::sync::Mutex::new(vec![]),
            };
            let (_cancel, cancel) = watch::channel(false);
            let (stop, stopped) = watch::channel(false);
            tokio::time::timeout(
                Duration::from_secs(2),
                run(
                    &audio,
                    &recognizer,
                    None,
                    None,
                    decoding,
                    cancel,
                    stopped,
                    Duration::from_millis(1),
                    |_| {
                        stop.send_replace(true);
                    },
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(*recognizer.seen.lock().unwrap(), [decoding]);
        }
    }

    #[tokio::test]
    async fn stop_or_cancel_during_inference_discards_late_text_and_drains_work() {
        for cancelling in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> =
                tokio::sync::Mutex::new(Box::new(Capture {
                    directory: directory.path().to_owned(),
                    snapshots: AtomicUsize::new(1),
                    stages: vec![],
                }));
            let release = Arc::new(Notify::new());
            let recognizer = Recognizer {
                calls: AtomicUsize::new(0),
                entered: Notify::new(),
                release: Some(Arc::clone(&release)),
            };
            let (cancel_tx, cancel) = watch::channel(false);
            let (stop, stopped) = watch::channel(false);
            let operation = run(
                &audio,
                &recognizer,
                None,
                None,
                asr::PreviewDecoding::Standard,
                cancel,
                stopped,
                Duration::from_millis(1),
                |_| panic!("late result reached target"),
            );
            let control = async {
                recognizer.entered.notified().await;
                if cancelling {
                    cancel_tx.send_replace(true);
                } else {
                    stop.send_replace(true);
                }
                // Capture remains independently accessible while inference is running.
                assert_eq!(audio.lock().await.state(), CaptureState::Capturing);
                release.notify_one();
            };
            let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(operation, control)
            })
            .await
            .unwrap();
            result.unwrap();
            assert_eq!(recognizer.calls.load(Ordering::SeqCst), 1);
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn utterance_endpoint_advances_audio_without_deduplicating_repeated_words() {
        let directory = tempfile::tempdir().unwrap();
        let mut utterance: Vec<f32> = (0..28_800).map(|i| (i as f32 * 0.1).sin() * 0.2).collect();
        utterance.extend(vec![0.0; 19_200]);
        let source = [utterance.clone(), utterance].concat();
        let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> =
            tokio::sync::Mutex::new(Box::new(Capture {
                directory: directory.path().to_owned(),
                snapshots: AtomicUsize::new(0),
                stages: vec![stage(source)],
            }));
        let recognizer = Recognizer {
            calls: AtomicUsize::new(0),
            entered: Notify::new(),
            release: None,
        };
        let (_cancel, cancel) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let mut updates = vec![];
        tokio::time::timeout(
            Duration::from_secs(2),
            run(
                &audio,
                &recognizer,
                None,
                None,
                asr::PreviewDecoding::Standard,
                cancel,
                stopped,
                Duration::from_millis(1),
                |text| {
                    updates.push(text.to_owned());
                    if updates.len() == 2 {
                        stop.send_replace(true);
                    }
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updates, [revise("途中"), revise("途中途中の文章")]);
        assert_eq!(recognizer.calls.load(Ordering::SeqCst), 2);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    fn transcript(text: &str) -> asr::Transcript {
        asr::Transcript {
            text: text.into(),
            segments: vec![],
            model: "test".into(),
            duration_ms: 1,
        }
    }

    #[test]
    fn unrecognizable_windows_are_skipped_but_real_failures_end_live_mode() {
        for empty in ["", "  \n"] {
            assert_eq!(window_text(Ok(transcript(empty))), Ok(None));
        }
        assert_eq!(
            window_text(Ok(transcript(" 途中 "))),
            Ok(Some(" 途中 ".into()))
        );
        for error in [asr::AsrError::Crashed, asr::AsrError::Timeout] {
            assert!(window_text(Err(error)).is_err());
        }

        assert!(matches!(
            window_artifact(Err(AudioError::NoVoiceDetected)),
            Ok(None)
        ));
        assert!(matches!(
            window_artifact(Err(AudioError::TooShort {
                actual: Duration::from_millis(10),
                minimum: Duration::from_millis(100),
            })),
            Ok(None)
        ));
        assert!(window_artifact(Err(AudioError::NotCapturing)).is_err());
    }

    #[test]
    fn an_empty_endpoint_withdraws_the_uncommitted_partial() {
        assert_eq!(
            window_display("前文", Some("途中"), false),
            Some(revise("前文途中"))
        );
        // An empty partial keeps whatever is displayed.
        assert_eq!(window_display("前文", None, false), None);
        // An empty endpoint retracts to the committed text, even when empty.
        assert_eq!(
            window_display("前文", None, true),
            Some(LiveText::Retract("前文".into()))
        );
        assert_eq!(
            window_display("", None, true),
            Some(LiveText::Retract(String::new()))
        );
    }

    /// Returns the scripted results in order, then repeats the last one.
    struct ScriptedRecognizer {
        calls: AtomicUsize,
        script: Vec<Option<&'static str>>,
    }
    #[async_trait::async_trait]
    impl Transcriber for ScriptedRecognizer {
        async fn load(&self, _: &str) -> Result<(), asr::AsrError> {
            Ok(())
        }
        async fn transcribe(
            &self,
            _: &Path,
            _: Option<&str>,
            _: watch::Receiver<bool>,
        ) -> Result<asr::Transcript, asr::AsrError> {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            self.script[index.min(self.script.len() - 1)]
                .map(transcript)
                .ok_or(asr::AsrError::Crashed)
        }
        async fn shutdown(&self) -> Result<(), asr::AsrError> {
            Ok(())
        }
        async fn reconfigure(&self, _: WorkerCommand) {}
    }

    fn voiced_capture(directory: &Path) -> tokio::sync::Mutex<Box<dyn AudioCapture>> {
        tokio::sync::Mutex::new(Box::new(Capture {
            directory: directory.to_owned(),
            snapshots: AtomicUsize::new(0),
            stages: vec![],
        }))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_live_updates_keep_runtime_responsive_and_stop_joins_the_owner() {
        for cancelling in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().to_owned();
            let runtime_thread = std::thread::current().id();
            let (entered, waiting) = tokio::sync::oneshot::channel();
            let (release, blocked) = std::sync::mpsc::channel();
            let (stop, stopped) = watch::channel(false);
            let (cancel_tx, cancel) = watch::channel(false);
            let owner = spawn_recording_owner(async move {
                let audio = voiced_capture(&path);
                let recognizer = ScriptedRecognizer {
                    calls: AtomicUsize::new(0),
                    script: vec![Some("live sample")],
                };
                let mut entered = Some(entered);
                let blocked = std::sync::Mutex::new(blocked);
                let mut updates = 0;
                let result = run(
                    &audio,
                    &recognizer,
                    None,
                    None,
                    asr::PreviewDecoding::Standard,
                    cancel,
                    stopped,
                    Duration::from_millis(1),
                    |_| {
                        updates += 1;
                        assert_ne!(std::thread::current().id(), runtime_thread);
                        entered.take().unwrap().send(()).unwrap();
                        // Stand in for a slow synchronous UIA/clipboard operation.
                        blocked
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(3))
                            .unwrap();
                    },
                )
                .await;
                (result, updates, recognizer.calls.load(Ordering::SeqCst))
            });
            tokio::time::timeout(Duration::from_secs(3), waiting)
                .await
                .unwrap()
                .unwrap();
            // A single runtime thread must still process timers and cancellation
            // while the update is blocked, with no additional inference queued.
            tokio::time::timeout(
                Duration::from_millis(500),
                tokio::time::sleep(Duration::from_millis(10)),
            )
            .await
            .unwrap();
            if cancelling {
                cancel_tx.send_replace(true);
            } else {
                stop.send_replace(true);
            }
            assert!(!owner.is_finished());
            release.send(()).unwrap();
            let (result, updates, calls) = tokio::time::timeout(Duration::from_secs(3), owner)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result, Ok(()));
            assert_eq!((updates, calls), (1, 1));
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn a_stop_signalled_before_audio_finalization_starts_no_new_decode() {
        for signalled in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let audio = Arc::new(voiced_capture(directory.path()));
            let recognizer = Arc::new(ScriptedRecognizer {
                calls: AtomicUsize::new(0),
                script: vec![Some("途中")],
            });
            let (_cancel, cancel) = watch::channel(false);
            let (stop, stopped) = watch::channel(false);
            // Stop holds the capture while it finalizes the recording.
            let finalizing = audio.lock().await;
            let task = tokio::spawn({
                let audio = Arc::clone(&audio);
                let recognizer = Arc::clone(&recognizer);
                async move {
                    let result = run(
                        &audio,
                        recognizer.as_ref(),
                        None,
                        None,
                        asr::PreviewDecoding::Standard,
                        cancel,
                        stopped,
                        Duration::from_millis(1),
                        |_| {},
                    )
                    .await;
                    assert_eq!(result, Ok(()));
                    deferred_test_draft()
                }
            });
            let task = LiveTask { stop, task };
            // Let the loop wait for the capture held by the stop path.
            tokio::time::sleep(Duration::from_millis(30)).await;
            if signalled {
                task.signal_stop();
            }
            drop(finalizing);
            if !signalled {
                // Without the early signal the waiting loop decodes again.
                tokio::time::timeout(Duration::from_secs(2), async {
                    while recognizer.calls.load(Ordering::SeqCst) == 0 {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                })
                .await
                .unwrap();
            }
            // Joining still returns the draft for finalization.
            tokio::time::timeout(Duration::from_secs(2), task.finish())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                recognizer.calls.load(Ordering::SeqCst) == 0,
                signalled,
                "signalled: {signalled}"
            );
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn an_empty_partial_result_does_not_end_live_recognition() {
        let directory = tempfile::tempdir().unwrap();
        let audio = voiced_capture(directory.path());
        let recognizer = ScriptedRecognizer {
            calls: AtomicUsize::new(0),
            script: vec![Some(""), Some("途中")],
        };
        let (_cancel, cancel) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let mut updates = vec![];
        tokio::time::timeout(
            Duration::from_secs(2),
            run(
                &audio,
                &recognizer,
                None,
                None,
                asr::PreviewDecoding::Standard,
                cancel,
                stopped,
                Duration::from_millis(1),
                |text| {
                    updates.push(text.to_owned());
                    stop.send_replace(true);
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        // The empty partial neither ended live mode nor retracted anything.
        assert_eq!(updates, [revise("途中")]);
        assert_eq!(recognizer.calls.load(Ordering::SeqCst), 2);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    fn revise(text: &str) -> LiveText {
        LiveText::Revise(text.into())
    }

    /// Runs live recognition over a partial window followed by an utterance
    /// endpoint, until the first retraction or the timeout.
    async fn partial_then_endpoint(
        endpoint: Stage,
        script: Vec<Option<&'static str>>,
    ) -> (Vec<LiveText>, usize) {
        let directory = tempfile::tempdir().unwrap();
        // A partial window of ongoing speech, then the same speech followed by
        // enough silence to end the utterance.
        let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> =
            tokio::sync::Mutex::new(Box::new(Capture {
                directory: directory.path().to_owned(),
                snapshots: AtomicUsize::new(0),
                stages: vec![stage(tone(1.6)), endpoint],
            }));
        let recognizer = ScriptedRecognizer {
            calls: AtomicUsize::new(0),
            script,
        };
        let (_cancel, cancel) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let mut updates = vec![];
        tokio::time::timeout(
            Duration::from_secs(2),
            run(
                &audio,
                &recognizer,
                None,
                None,
                asr::PreviewDecoding::Standard,
                cancel,
                stopped,
                Duration::from_millis(1),
                |live| {
                    updates.push(live.to_owned());
                    if matches!(live, LiveText::Retract(_)) {
                        stop.send_replace(true);
                    }
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        (updates, recognizer.calls.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn an_empty_endpoint_retracts_the_partial_already_shown() {
        let endpoint = stage([tone(1.6), vec![0.0; 24_000]].concat());
        let (updates, calls) = partial_then_endpoint(endpoint, vec![Some("途中"), Some("")]).await;
        // The preview and the target draft both drop the uncommitted partial.
        assert_eq!(updates, [revise("途中"), LiveText::Retract(String::new())]);
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn a_silent_endpoint_window_retracts_the_partial_already_shown() {
        // The endpoint window (speech plus padding) is shorter than this
        // snapshot accepts, so it is never decoded.
        let endpoint = Stage {
            samples: [tone(1.6), vec![0.0; 72_000]].concat(),
            minimum_duration: Duration::from_secs(2),
        };
        let (updates, calls) = partial_then_endpoint(endpoint, vec![Some("途中")]).await;
        assert_eq!(updates, [revise("途中"), LiveText::Retract(String::new())]);
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn a_worker_failure_still_ends_live_recognition() {
        let directory = tempfile::tempdir().unwrap();
        let audio = voiced_capture(directory.path());
        let recognizer = ScriptedRecognizer {
            calls: AtomicUsize::new(0),
            script: vec![Some(""), None],
        };
        let (_cancel, cancel) = watch::channel(false);
        let (_stop, stopped) = watch::channel(false);
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            run(
                &audio,
                &recognizer,
                None,
                None,
                asr::PreviewDecoding::Standard,
                cancel,
                stopped,
                Duration::from_millis(1),
                |_| panic!("no text was recognized"),
            ),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert_eq!(recognizer.calls.load(Ordering::SeqCst), 2);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    fn probe(window_ms: u64, tail: Vec<f32>) -> audio::AudioProbe {
        audio::AudioProbe::for_test(
            Duration::from_millis(window_ms),
            tail,
            &CaptureConfig::default(),
        )
    }

    fn silence(ms: usize) -> Vec<f32> {
        vec![0.0; ms * 24]
    }

    fn speech(ms: usize) -> Vec<f32> {
        tone(ms as f64 / 1000.0)
    }

    #[test]
    fn the_gate_prepares_whenever_a_wait_could_change() {
        let segmentation = Segmentation::default();
        // Segmentation after a partial decode of the first `until` ms.
        let partial = |until: u64| {
            let mut segmentation = Segmentation::default();
            let until = Duration::from_millis(until);
            segmentation.recognized(until, until, Duration::ZERO, false);
            segmentation
        };
        let mut gate = PreparationGate::default();
        // Without a waiting preparation every iteration prepares.
        assert!(gate.should_prepare(&probe(800, silence(800)), &segmentation));

        // Speech continues: 1 s window, no trailing silence at the last Wait.
        gate.waited(Duration::from_millis(1_000), Duration::ZERO);
        assert_eq!(gate.unseen_from(), Some(Duration::from_millis(1_000)));
        // Nothing new: the same audio would plan the same Wait.
        assert!(!gate.should_prepare(&probe(1_000, vec![]), &segmentation));
        // Certainly voiced new audio cannot end the utterance yet.
        assert!(!gate.should_prepare(&probe(1_150, speech(150)), &segmentation));
        // A decode may be due once the window holds 1.2 s of new context.
        assert!(gate.should_prepare(&probe(1_170, speech(170)), &segmentation));

        // Quiet new audio may complete the endpoint silence: 650 ms, or 350 ms
        // once the window could hold a 12-second utterance.
        gate.waited(Duration::from_millis(600), Duration::from_millis(100));
        assert!(!gate.should_prepare(&probe(1_000, silence(400)), &segmentation));
        assert!(gate.should_prepare(&probe(1_110, silence(510)), &segmentation));
        gate.waited(Duration::from_secs(12), Duration::from_millis(100));
        assert!(!gate.should_prepare(&probe(12_150, silence(150)), &partial(12_000)));
        assert!(gate.should_prepare(&probe(12_210, silence(210)), &partial(12_000)));
        gate.waited(Duration::from_millis(600), Duration::from_millis(100));
        // Voice ending the new audio restarts the silence count.
        let mut tail = silence(100);
        tail.extend(speech(200));
        tail.extend(silence(100));
        assert!(!gate.should_prepare(&probe(1_000, tail), &segmentation));
        // Never more than the maximum deferral of audio without preparing.
        let long_gap = partial(20_000);
        gate.waited(Duration::from_secs(20), Duration::ZERO);
        assert!(!gate.should_prepare(&probe(20_990, speech(990)), &long_gap));
        assert!(gate.should_prepare(&probe(21_000, speech(1_000)), &long_gap));

        gate.reset();
        assert_eq!(gate.unseen_from(), None);
    }

    /// Audio grows by one live interval per loop iteration: speech after a
    /// short silence, then silence. With `probes` off the capture cannot be
    /// probed, which is the loop before the preparation gate.
    struct GrowingCapture {
        directory: PathBuf,
        probes: bool,
        signal: Vec<f32>,
        ticks: Arc<AtomicUsize>,
        probed: std::sync::atomic::AtomicBool,
        snapshots: Arc<AtomicUsize>,
        /// Raw samples copied (and so enhanced) by all snapshots.
        copied: Arc<AtomicUsize>,
    }

    impl GrowingCapture {
        const STEP: usize = 3_600; // 150 ms at 24 kHz.

        fn new(
            directory: &Path,
            probes: bool,
            ticks: Arc<AtomicUsize>,
            snapshots: Arc<AtomicUsize>,
            copied: Arc<AtomicUsize>,
        ) -> Self {
            let mut signal = silence(300);
            signal.extend(speech(4_000));
            signal.extend(silence(1_500));
            Self {
                directory: directory.to_owned(),
                probes,
                signal,
                ticks,
                probed: false.into(),
                snapshots,
                copied,
            }
        }

        fn config(&self) -> CaptureConfig {
            CaptureConfig {
                artifact_directory: Some(self.directory.clone()),
                ..CaptureConfig::default()
            }
        }

        /// The recording so far, advancing once per loop iteration.
        fn recorded(&self, advance: bool) -> Result<&[f32], AudioError> {
            let tick = if advance {
                self.ticks.fetch_add(1, Ordering::SeqCst) + 1
            } else {
                self.ticks.load(Ordering::SeqCst)
            };
            let length = tick * Self::STEP;
            if length > self.signal.len() {
                return Err(AudioError::NotCapturing);
            }
            Ok(&self.signal[..length])
        }
    }

    fn index(at: Duration) -> usize {
        (at.as_secs_f64() * 24_000.0).round() as usize
    }

    impl AudioCapture for GrowingCapture {
        fn snapshot(&self, since: Duration) -> Result<AudioSnapshot, AudioError> {
            let advance = !self.probed.swap(false, Ordering::SeqCst);
            let recorded = self.recorded(advance)?;
            let samples = recorded[index(since).min(recorded.len())..].to_vec();
            self.snapshots.fetch_add(1, Ordering::SeqCst);
            self.copied.fetch_add(samples.len(), Ordering::SeqCst);
            Ok(AudioSnapshot::for_test(samples, self.config()))
        }
        fn probe(
            &self,
            since: Duration,
            new_from: Duration,
        ) -> Result<Option<audio::AudioProbe>, AudioError> {
            self.probed.store(true, Ordering::SeqCst);
            let recorded = self.recorded(true)?;
            if !self.probes {
                return Ok(None);
            }
            let start = index(since).min(recorded.len());
            let tail = index(new_from).clamp(start, recorded.len());
            Ok(Some(audio::AudioProbe::for_test(
                Duration::from_secs_f64((recorded.len() - start) as f64 / 24_000.0),
                recorded[tail..].to_vec(),
                &self.config(),
            )))
        }
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
        fn stop(&mut self) -> AudioFuture<'_, AudioArtifact> {
            panic!("live inference must not stop capture")
        }
        fn cancel(&mut self) -> AudioFuture<'_, ()> {
            panic!("live inference must not cancel capture")
        }
        fn state(&self) -> CaptureState {
            CaptureState::Capturing
        }
        fn level(&self) -> LevelMeter {
            LevelMeter::default()
        }
    }

    /// Records the iteration and the exact WAV bytes of every decode.
    struct RecordingRecognizer {
        ticks: Arc<AtomicUsize>,
        decodes: std::sync::Mutex<Vec<(usize, Vec<u8>)>>,
    }
    #[async_trait::async_trait]
    impl Transcriber for RecordingRecognizer {
        async fn load(&self, _: &str) -> Result<(), asr::AsrError> {
            Ok(())
        }
        async fn transcribe(
            &self,
            path: &Path,
            _: Option<&str>,
            _: watch::Receiver<bool>,
        ) -> Result<asr::Transcript, asr::AsrError> {
            let mut decodes = self.decodes.lock().unwrap();
            decodes.push((self.ticks.load(Ordering::SeqCst), fs::read(path).unwrap()));
            Ok(asr::Transcript {
                text: format!("window {}", decodes.len()),
                segments: vec![],
                model: "test".into(),
                duration_ms: 1,
            })
        }
        async fn shutdown(&self) -> Result<(), asr::AsrError> {
            Ok(())
        }
        async fn reconfigure(&self, _: WorkerCommand) {}
    }

    /// Decodes (iteration, WAV bytes), then preparations, samples prepared
    /// and iterations.
    async fn replay(probes: bool) -> (Vec<(usize, Vec<u8>)>, usize, usize, usize) {
        let directory = tempfile::tempdir().unwrap();
        let ticks = Arc::new(AtomicUsize::new(0));
        let snapshots = Arc::new(AtomicUsize::new(0));
        let copied = Arc::new(AtomicUsize::new(0));
        let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> =
            tokio::sync::Mutex::new(Box::new(GrowingCapture::new(
                directory.path(),
                probes,
                Arc::clone(&ticks),
                Arc::clone(&snapshots),
                Arc::clone(&copied),
            )));
        let recognizer = RecordingRecognizer {
            ticks: Arc::clone(&ticks),
            decodes: std::sync::Mutex::new(vec![]),
        };
        let (_cancel, cancel) = watch::channel(false);
        let (_stop, stopped) = watch::channel(false);
        tokio::time::timeout(
            Duration::from_secs(10),
            run(
                &audio,
                &recognizer,
                None,
                None,
                asr::PreviewDecoding::Standard,
                cancel,
                stopped,
                Duration::from_millis(1),
                |_| {},
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let decodes = recognizer.decodes.into_inner().unwrap();
        (
            decodes,
            snapshots.load(Ordering::SeqCst),
            copied.load(Ordering::SeqCst),
            ticks.load(Ordering::SeqCst),
        )
    }

    #[tokio::test]
    async fn the_gate_skips_preparations_without_changing_any_decoded_window() {
        let (ungated, ungated_snapshots, ungated_copied, ticks) = replay(false).await;
        let (gated, gated_snapshots, gated_copied, gated_ticks) = replay(true).await;
        assert_eq!(ticks, gated_ticks);
        // Partial windows during speech plus the utterance endpoint.
        assert!(ungated.len() >= 3, "{} decodes", ungated.len());
        // Every decode happens on the same iteration with byte-identical audio.
        assert_eq!(gated.len(), ungated.len());
        for ((gated_tick, gated_wav), (tick, wav)) in gated.iter().zip(&ungated) {
            assert_eq!(gated_tick, tick);
            assert!(gated_wav == wav, "decode at iteration {tick} changed");
        }
        // Preparations that remain are mostly short windows (leading
        // silence, the endpoint); the long ones during speech are skipped.
        eprintln!(
            "{gated_snapshots} of {ungated_snapshots} preparations,              {gated_copied} of {ungated_copied} samples"
        );
        assert!(gated_snapshots < ungated_snapshots);
        assert!(gated_copied * 2 <= ungated_copied);
    }
}
