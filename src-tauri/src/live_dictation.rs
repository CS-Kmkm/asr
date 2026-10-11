use super::*;
use audio::AudioError;
use injection::ProvisionalInsertion;
use tokio::sync::watch;
#[cfg(test)]
mod benchmark;
mod segmentation;
use segmentation::{append_utterance, Decision, Segmentation};

const LIVE_INTERVAL: Duration = Duration::from_millis(150);

/// Default Dictate pastes only the final text once. Live provisional target
/// insertion is an explicit opt-in because editors whose content cannot be
/// read back keep the first unverified fragment. Other modes always defer.
pub(crate) fn defers_target_insertion(mode: PipelineMode, settings: &Settings) -> bool {
    mode != PipelineMode::Dictate || !settings.live_target_insertion
}

#[derive(Clone, Copy)]
struct SettleTiming {
    quiet: Duration,
    limit: Duration,
    poll: Duration,
}

/// Clicking, scrolling or typing during recording and recognition is normal.
/// A deferred final paste waits for a short quiet moment instead.
const SETTLE_TIMING: SettleTiming = SettleTiming {
    quiet: Duration::from_millis(350),
    limit: Duration::from_secs(5),
    poll: Duration::from_millis(50),
};

/// Returns a checkpoint taken once input has been quiet for `timing.quiet`,
/// preferring a moment when `ready` also holds. At the limit, a quiet but
/// unready target still yields a checkpoint so the outcome names the target
/// as the reason. `None` means input never settled, the monitor stopped, or
/// the operation was cancelled; the caller keeps its earlier checkpoint.
async fn settled_checkpoint(
    monitor: &InputMonitor,
    cancel: &watch::Receiver<bool>,
    timing: SettleTiming,
    ready: impl Fn() -> bool,
) -> Option<u64> {
    let started = Instant::now();
    let mut last = monitor.checkpoint()?;
    let mut quiet_since = Instant::now();
    loop {
        if *cancel.borrow() {
            return None;
        }
        let current = monitor.checkpoint()?;
        if current != last || monitor.shortcut_pending() {
            last = current;
            quiet_since = Instant::now();
        }
        if quiet_since.elapsed() >= timing.quiet && ready() {
            // Input during the readiness check would make this checkpoint stale.
            let after = monitor.checkpoint()?;
            if after == current {
                return Some(current);
            }
            last = after;
            quiet_since = Instant::now();
        }
        if started.elapsed() >= timing.limit {
            return (quiet_since.elapsed() >= timing.quiet).then_some(last);
        }
        tokio::time::sleep(timing.poll).await;
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
    task: tauri::async_runtime::JoinHandle<LiveDraft>,
}

impl LiveTask {
    pub(crate) async fn finish(self) -> Result<LiveDraft, String> {
        self.stop.send_replace(true);
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
        }
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
    pub(crate) async fn settle_before_final(&mut self, cancel: &watch::Receiver<bool>) {
        if !self.defer_insertion || self.pasted || self.session.is_some() {
            return;
        }
        let ready =
            |draft: &Self| draft.injector.target_ready(&draft.target) && draft.focus_unchanged();
        if self
            .checkpoint
            .is_some_and(|checkpoint| self.monitor.unchanged_since(checkpoint))
            && ready(self)
        {
            return;
        }
        let draft = &*self;
        let settled =
            settled_checkpoint(&draft.monitor, cancel, SETTLE_TIMING, || ready(draft)).await;
        if let Some(checkpoint) = settled {
            self.renew_checkpoint(checkpoint);
        }
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
) -> LiveTask {
    draft.monitor.observe_cancellation(Some(cancel.clone()));
    let (stop, stopped) = watch::channel(false);
    let task = tauri::async_runtime::spawn(async move {
        let services = app.state::<Services>();
        let result = run(
            &services.audio,
            services.transcriber.as_ref(),
            prompt.as_deref(),
            speech_locale.as_deref(),
            cancel,
            stopped,
            LIVE_INTERVAL,
            |live| {
                if services.lifecycle.phase() != PipelinePhase::Recording
                    || services.lifecycle.is_cancelled(operation_id)
                {
                    return;
                }
                let state = app.state::<AppState>();
                let _ = app.emit("app-state", state.publish_result(live.text().to_owned()));
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
/// queue. Stop drains the current request; cancellation uses the worker's
/// existing cancellation protocol. A late result is never published.
async fn run(
    audio: &tokio::sync::Mutex<Box<dyn AudioCapture>>,
    transcriber: &dyn Transcriber,
    prompt: Option<&str>,
    speech_locale: Option<&str>,
    mut cancel: watch::Receiver<bool>,
    mut stopped: watch::Receiver<bool>,
    interval: Duration,
    mut on_update: impl FnMut(&LiveText),
) -> Result<(), LiveFailure> {
    let mut segmentation = Segmentation::default();
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
        let snapshot = {
            let audio = audio.lock().await;
            if *cancel.borrow() || *stopped.borrow() {
                return Ok(());
            }
            match audio.snapshot(cursor) {
                Ok(snapshot) => snapshot,
                Err(AudioError::NotCapturing) => return Ok(()),
                Err(_) => return Err(LiveFailure::Capture),
            }
        };
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
            Decision::Wait => continue,
            Decision::Decode {
                samples,
                endpoint,
                voiced_until,
            } => (samples, endpoint, voiced_until),
        };
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
                    .transcribe_with_locale(&artifact.path, prompt, speech_locale, cancel.clone())
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
    use audio::{AudioArtifact, AudioFuture, AudioSnapshot, CaptureState, LevelMeter};
    use injection::test_backend::{self, MockBackend};
    use std::sync::atomic::{AtomicUsize, Ordering};
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
        }
    }

    #[test]
    fn dictate_defers_target_input_unless_live_insertion_is_enabled() {
        let mut settings = Settings::default();
        assert!(defers_target_insertion(PipelineMode::Dictate, &settings));
        assert!(defers_target_insertion(PipelineMode::Translate, &settings));
        settings.live_target_insertion = true;
        assert!(!defers_target_insertion(PipelineMode::Dictate, &settings));
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
    };

    fn available_monitor() -> Arc<InputMonitor> {
        let monitor = Arc::new(InputMonitor::default());
        monitor.test_set_available(true);
        monitor
    }

    #[tokio::test]
    async fn clicks_during_recording_settle_to_a_fresh_checkpoint() {
        let mut draft = deferred_test_draft();
        draft.monitor.test_record_input();
        assert!(!draft.can_insert_final("final"));
        let (_cancel, cancelled) = watch::channel(false);
        let checkpoint = settled_checkpoint(&draft.monitor, &cancelled, FAST, || true)
            .await
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
        // Always runnable, so it records input between any two polls on this
        // single-threaded runtime, independent of timer resolution or stalls.
        let typing = tokio::spawn(async move {
            while !stop.load(Ordering::Acquire) {
                busy.test_record_input();
                tokio::task::yield_now().await;
            }
        });
        let (_cancel, cancelled) = watch::channel(false);
        let result = settled_checkpoint(&monitor, &cancelled, FAST, || true).await;
        done.store(true, Ordering::Release);
        typing.await.unwrap();
        assert_eq!(result, None);
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
            tokio::time::timeout(
                Duration::from_millis(50),
                draft.settle_before_final(&cancelled),
            )
            .await
            .unwrap();
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
        let checkpoint = settled_checkpoint(&monitor, &cancelled, FAST, || false).await;
        assert_eq!(checkpoint, monitor.checkpoint());
        assert!(started.elapsed() >= FAST.limit);
    }

    #[tokio::test]
    async fn cancellation_or_a_stopped_monitor_ends_the_wait_without_a_checkpoint() {
        let monitor = available_monitor();
        let (cancel, cancelled) = watch::channel(false);
        cancel.send_replace(true);
        assert_eq!(
            settled_checkpoint(&monitor, &cancelled, FAST, || true).await,
            None
        );
        let (_cancel, cancelled) = watch::channel(false);
        monitor.test_set_available(false);
        assert_eq!(
            settled_checkpoint(&monitor, &cancelled, FAST, || true).await,
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
}
