use std::sync::{Mutex, RwLock};

use chrono::Utc;
use tokio::sync::watch;

use crate::types::{AppPhase, AppStage, AppStateSnapshot, OperationKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelinePhase {
    Idle,
    Starting,
    Recording,
    Processing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelineMode {
    Dictate,
    Translate,
    Edit,
    Ask,
    /// Selected-text translation: no recording, it begins in Processing.
    SelectedText,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PipelinePurpose {
    Input,
    DictationTest,
    InsertionTest,
}

struct PipelineOperation {
    id: u64,
    mode: PipelineMode,
    phase: PipelinePhase,
    cancel: watch::Sender<bool>,
    /// Set once an irreversible side effect succeeds. The operation keeps the
    /// lifecycle claimed until its owner publishes completion and finishes,
    /// but it can no longer be cancelled.
    committed: bool,
    purpose: PipelinePurpose,
}

pub struct PipelineLifecycle {
    inner: Mutex<Option<PipelineOperation>>,
    next_id: std::sync::atomic::AtomicU64,
    /// Every phase change, published under `inner`'s lock so observers see
    /// them in order (for example the cancel hotkey registration).
    phase_changes: watch::Sender<PipelinePhase>,
}

impl Default for PipelineLifecycle {
    fn default() -> Self {
        Self {
            inner: Mutex::new(None),
            next_id: std::sync::atomic::AtomicU64::new(1),
            phase_changes: watch::channel(PipelinePhase::Idle).0,
        }
    }
}

impl PipelineLifecycle {
    /// Observes the lifecycle phase; the current phase is marked unseen.
    pub fn subscribe_phase(&self) -> watch::Receiver<PipelinePhase> {
        let mut receiver = self.phase_changes.subscribe();
        receiver.mark_changed();
        receiver
    }

    fn publish_phase(&self, operation: &Option<PipelineOperation>) {
        let phase = operation
            .as_ref()
            .map_or(PipelinePhase::Idle, |operation| operation.phase);
        self.phase_changes.send_if_modified(|current| {
            let changed = *current != phase;
            *current = phase;
            changed
        });
    }

    /// Claims the lifecycle for an operation that records nothing (a History
    /// Retry or a selected-text translation): it begins in Processing, so a
    /// voice start is refused and a cancel is left to this owner, which
    /// publishes the terminal phase and calls `finish`.
    pub fn begin_non_recording(
        &self,
        mode: PipelineMode,
    ) -> Result<(u64, watch::Receiver<bool>), &'static str> {
        self.begin_non_recording_for(mode, PipelinePurpose::Input)
    }

    pub(crate) fn begin_insertion_test(
        &self,
    ) -> Result<(u64, watch::Receiver<bool>), &'static str> {
        self.begin_non_recording_for(PipelineMode::SelectedText, PipelinePurpose::InsertionTest)
    }

    fn begin_non_recording_for(
        &self,
        mode: PipelineMode,
        purpose: PipelinePurpose,
    ) -> Result<(u64, watch::Receiver<bool>), &'static str> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "pipeline lifecycle is unavailable")?;
        if inner.is_some() {
            return Err("a recording or processing operation is already active");
        }
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (cancel, receiver) = watch::channel(false);
        *inner = Some(PipelineOperation {
            id,
            mode,
            phase: PipelinePhase::Processing,
            cancel,
            committed: false,
            purpose,
        });
        self.publish_phase(&inner);
        Ok((id, receiver))
    }

    pub fn begin_start(&self, mode: PipelineMode) -> Result<u64, &'static str> {
        self.begin_start_for(mode, false)
    }

    pub(crate) fn begin_dictation_test(&self) -> Result<u64, &'static str> {
        self.begin_start_for(PipelineMode::Dictate, true)
    }

    pub(crate) fn is_dictation_test(&self, id: u64) -> bool {
        self.inner.lock().is_ok_and(|inner| {
            inner.as_ref().is_some_and(|operation| {
                operation.id == id && operation.purpose == PipelinePurpose::DictationTest
            })
        })
    }

    pub(crate) fn dictation_test_id(&self) -> Option<u64> {
        self.inner.lock().ok().and_then(|inner| {
            inner
                .as_ref()
                .filter(|operation| operation.purpose == PipelinePurpose::DictationTest)
                .map(|operation| operation.id)
        })
    }

    pub(crate) fn is_setup_test(&self, id: u64) -> bool {
        self.inner.lock().is_ok_and(|inner| {
            inner.as_ref().is_some_and(|operation| {
                operation.id == id && operation.purpose != PipelinePurpose::Input
            })
        })
    }

    fn begin_start_for(
        &self,
        mode: PipelineMode,
        dictation_test: bool,
    ) -> Result<u64, &'static str> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "pipeline lifecycle is unavailable")?;
        if inner.is_some() {
            return Err("a recording or processing operation is already active");
        }
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (cancel, _) = watch::channel(false);
        *inner = Some(PipelineOperation {
            id,
            mode,
            phase: PipelinePhase::Starting,
            cancel,
            committed: false,
            purpose: if dictation_test {
                PipelinePurpose::DictationTest
            } else {
                PipelinePurpose::Input
            },
        });
        self.publish_phase(&inner);
        Ok(id)
    }

    pub fn mark_recording(&self, id: u64) -> Result<(), &'static str> {
        self.transition(id, PipelinePhase::Starting, PipelinePhase::Recording)
    }

    pub fn cancellation(&self, id: u64) -> Result<watch::Receiver<bool>, &'static str> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| "pipeline lifecycle is unavailable")?;
        inner
            .as_ref()
            .filter(|op| op.id == id)
            .map(|op| op.cancel.subscribe())
            .ok_or("pipeline operation is no longer active")
    }

    pub fn begin_processing(
        &self,
    ) -> Result<(u64, PipelineMode, watch::Receiver<bool>), &'static str> {
        self.begin_processing_for(None)
    }

    pub fn begin_processing_for(
        &self,
        expected: Option<u64>,
    ) -> Result<(u64, PipelineMode, watch::Receiver<bool>), &'static str> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "pipeline lifecycle is unavailable")?;
        let operation = inner.as_mut().ok_or("no recording is active")?;
        if expected.is_some_and(|id| operation.id != id) {
            return Err("pipeline operation is no longer active");
        }
        if operation.phase != PipelinePhase::Recording {
            return Err("recording stop is already in progress");
        }
        if *operation.cancel.borrow() {
            return Err("pipeline operation was cancelled");
        }
        operation.phase = PipelinePhase::Processing;
        let claimed = (operation.id, operation.mode, operation.cancel.subscribe());
        self.publish_phase(&inner);
        Ok(claimed)
    }

    /// A held chord released during preparation must prevent a later mic
    /// open. The start owner performs cleanup and releases the lifecycle.
    pub fn cancel_start(&self, id: u64) -> bool {
        let Ok(inner) = self.inner.lock() else {
            return false;
        };
        let Some(operation) = inner
            .as_ref()
            .filter(|op| op.id == id && op.phase == PipelinePhase::Starting)
        else {
            return false;
        };
        operation.cancel.send_replace(true);
        true
    }

    pub fn cancel(&self) -> Result<Option<(u64, PipelinePhase)>, &'static str> {
        self.cancel_for(None)
    }

    pub(crate) fn cancel_for(
        &self,
        expected: Option<u64>,
    ) -> Result<Option<(u64, PipelinePhase)>, &'static str> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| "pipeline lifecycle is unavailable")?;
        let Some(operation) = inner.as_ref() else {
            return Ok(None);
        };
        if expected.is_some_and(|id| id != operation.id)
            || operation.committed
            || *operation.cancel.borrow()
        {
            return Ok(None);
        }
        operation.cancel.send_replace(true);
        Ok(Some((operation.id, operation.phase)))
    }

    pub fn finish(&self, id: u64) {
        if let Ok(mut inner) = self.inner.lock() {
            if inner.as_ref().is_some_and(|operation| operation.id == id) {
                *inner = None;
                self.publish_phase(&inner);
            }
        }
    }

    pub fn phase(&self) -> PipelinePhase {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.as_ref().map(|operation| operation.phase))
            .unwrap_or(PipelinePhase::Idle)
    }

    pub fn mode(&self) -> Option<PipelineMode> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.as_ref().map(|operation| operation.mode))
    }

    pub fn is_cancelled(&self, id: u64) -> bool {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| {
                inner
                    .as_ref()
                    .filter(|operation| operation.id == id)
                    .map(|operation| *operation.cancel.borrow())
            })
            .unwrap_or(true)
    }

    /// Hold the lifecycle decision across the final durable side effect. This
    /// makes cancellation either win before a search launches / Retry writes
    /// History or observe an already committed operation.
    ///
    /// A successful commit seals the operation instead of releasing it: a
    /// later cancel is a no-op, and the lifecycle stays claimed until the
    /// caller has published completion and calls `finish`. A new
    /// recording therefore cannot start and then be overwritten by that
    /// completion.
    /// The callback must not re-enter this lifecycle or wait for an async task.
    pub fn commit_side_effect<T, E>(
        &self,
        id: u64,
        commit: impl FnOnce() -> Result<T, E>,
    ) -> Result<Result<T, E>, &'static str> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "pipeline lifecycle is unavailable")?;
        let operation = inner
            .as_mut()
            .filter(|operation| {
                operation.id == id
                    && operation.phase == PipelinePhase::Processing
                    && !operation.committed
                    && !*operation.cancel.borrow()
            })
            .ok_or("pipeline operation was cancelled")?;
        let result = commit();
        if result.is_ok() {
            operation.committed = true;
        }
        Ok(result)
    }

    fn transition(
        &self,
        id: u64,
        from: PipelinePhase,
        to: PipelinePhase,
    ) -> Result<(), &'static str> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "pipeline lifecycle is unavailable")?;
        let operation = inner
            .as_mut()
            .ok_or("pipeline operation is no longer active")?;
        if operation.id != id || operation.phase != from || *operation.cancel.borrow() {
            return Err("pipeline operation was cancelled");
        }
        operation.phase = to;
        self.publish_phase(&inner);
        Ok(())
    }
}

pub struct AppState {
    inner: RwLock<AppStateSnapshot>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            inner: RwLock::new(AppStateSnapshot {
                phase: AppPhase::Idle,
                stage: AppStage::Idle,
                operation: None,
                operation_id: None,
                message: None,
                message_code: None,
                last_result: None,
                updated_at: Utc::now().to_rfc3339(),
            }),
        }
    }
}

impl AppState {
    pub fn snapshot(&self) -> AppStateSnapshot {
        self.inner.read().expect("app state lock poisoned").clone()
    }

    #[cfg(test)]
    pub fn transition(&self, phase: AppPhase, message: Option<String>) -> AppStateSnapshot {
        self.transition_stage(AppStage::for_phase(&phase), message)
    }

    pub fn transition_stage(&self, stage: AppStage, message: Option<String>) -> AppStateSnapshot {
        let mut state = self.inner.write().expect("app state lock poisoned");
        state.phase = stage.phase();
        state.stage = stage;
        if matches!(
            state.phase,
            AppPhase::Idle | AppPhase::Completed | AppPhase::Error
        ) {
            state.operation = None;
        }
        state.message = message;
        state.message_code = state
            .message
            .as_deref()
            .and_then(crate::messages::code)
            .map(str::to_owned);
        state.updated_at = Utc::now().to_rfc3339();
        state.clone()
    }

    pub fn set_operation(&self, operation: OperationKind, id: u64) {
        let mut state = self.inner.write().expect("app state lock poisoned");
        state.operation = Some(operation);
        state.operation_id = Some(id);
    }

    pub fn publish_result(&self, result: String) -> AppStateSnapshot {
        let mut state = self.inner.write().expect("app state lock poisoned");
        state.last_result = Some(result);
        state.updated_at = Utc::now().to_rfc3339();
        state.clone()
    }

    pub fn complete(&self, result: String, message: String) -> AppStateSnapshot {
        let mut state = self.inner.write().expect("app state lock poisoned");
        state.phase = AppPhase::Completed;
        state.stage = AppStage::Completed;
        state.operation = None;
        state.message = Some(message);
        state.message_code = state
            .message
            .as_deref()
            .and_then(crate::messages::code)
            .map(str::to_owned);
        state.last_result = Some(result);
        state.updated_at = Utc::now().to_rfc3339();
        state.clone()
    }

    pub fn clear_result(&self) {
        self.inner
            .write()
            .expect("app state lock poisoned")
            .last_result = None;
    }

    /// The published phase, or `None` if the state lock is poisoned. Unlike
    /// `snapshot`, this never panics, so a drop guard may call it.
    pub fn phase(&self) -> Option<AppPhase> {
        self.inner.read().ok().map(|state| state.phase.clone())
    }
}

/// Published when an operation ends early (an error return or a panic) while
/// the shared state still shows it busy. Every UI and the overlay recover
/// from it; it is an existing, localized message.
pub(crate) const OPERATION_INCOMPLETE: &str =
    "Recording could not be completed. Start a new recording and try again.";

/// Decides the terminal phase an exiting operation owner must still publish.
/// A busy phase (Recording, Processing, Injecting) must never outlive its
/// owner: a cancelled operation becomes Idle with its cancel message, any
/// other early exit becomes Error. A terminal phase the owner already
/// published (Idle, Completed, Error) is kept as is.
pub(crate) fn unfinished_exit_phase(
    current: &AppPhase,
    cancelled: bool,
    cancel_message: &'static str,
) -> Option<(AppPhase, &'static str)> {
    match current {
        AppPhase::Recording | AppPhase::Processing | AppPhase::Injecting => Some(if cancelled {
            (AppPhase::Idle, cancel_message)
        } else {
            (AppPhase::Error, OPERATION_INCOMPLETE)
        }),
        AppPhase::Idle | AppPhase::Completed | AppPhase::Error => None,
    }
}

/// RAII counterpart of `PipelineGuard` for the published phase. Declare it
/// after the `PipelineGuard` of the same operation: locals drop in reverse
/// order, so this publishes while the operation still owns the lifecycle and
/// a newer operation's phase can never be overwritten.
pub(crate) struct TerminalPhaseGuard<'a, P: FnMut(AppPhase, &'static str)> {
    state: &'a AppState,
    lifecycle: &'a PipelineLifecycle,
    id: u64,
    cancel_message: &'static str,
    publish: P,
}

impl<'a, P: FnMut(AppPhase, &'static str)> TerminalPhaseGuard<'a, P> {
    pub(crate) fn new(
        state: &'a AppState,
        lifecycle: &'a PipelineLifecycle,
        id: u64,
        cancel_message: &'static str,
        publish: P,
    ) -> Self {
        Self {
            state,
            lifecycle,
            id,
            cancel_message,
            publish,
        }
    }
}

impl<P: FnMut(AppPhase, &'static str)> Drop for TerminalPhaseGuard<'_, P> {
    fn drop(&mut self) {
        let Some(current) = self.state.phase() else {
            return;
        };
        let cancelled = self.lifecycle.is_cancelled(self.id);
        if let Some((phase, message)) =
            unfinished_exit_phase(&current, cancelled, self.cancel_message)
        {
            (self.publish)(phase, message);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hold_release_is_identity_checked_across_start_record_and_new_operations() {
        let lifecycle = super::PipelineLifecycle::default();
        let first = lifecycle.begin_start(super::PipelineMode::Dictate).unwrap();
        assert!(lifecycle.cancel_start(first));
        assert!(lifecycle.mark_recording(first).is_err());
        lifecycle.finish(first);
        let next = lifecycle.begin_start(super::PipelineMode::Ask).unwrap();
        assert!(!lifecycle.cancel_start(first));
        lifecycle.mark_recording(next).unwrap();
        assert!(lifecycle.begin_processing_for(Some(first)).is_err());
        assert_eq!(lifecycle.phase(), super::PipelinePhase::Recording);
        assert!(!lifecycle.cancel_start(next));
        assert_eq!(lifecycle.begin_processing_for(Some(next)).unwrap().0, next);
        assert!(lifecycle.begin_processing_for(Some(next)).is_err());
        assert!(!lifecycle.is_cancelled(next));
    }
    use super::*;

    #[test]
    fn starts_idle_without_sensitive_message() {
        let state = AppState::default().snapshot();
        assert_eq!(state.phase, AppPhase::Idle);
        assert_eq!(state.message, None);
        assert_eq!(state.last_result, None);
    }

    #[test]
    fn typed_stages_keep_retry_identity_and_clear_it_at_the_terminal_phase() {
        let state = AppState::default();
        state.set_operation(OperationKind::Retry, 7);
        let next = state.transition_stage(AppStage::Transcribing, Some("changed wording".into()));
        let json = serde_json::to_value(&next).unwrap();
        assert_eq!(json["stage"], "transcribing");
        assert_eq!(json["operation"], "retry");
        assert_eq!(json["phase"], "processing");
        let done = state.complete("result".into(), "Done.".into());
        assert_eq!(done.stage, AppStage::Completed);
        assert_eq!(done.operation, None);
    }

    #[test]
    fn transition_updates_phase_and_safe_status_message() {
        let state = AppState::default();
        let next = state.transition(AppPhase::Processing, Some("ASR worker started".into()));
        assert_eq!(next.phase, AppPhase::Processing);
        assert_eq!(next.message.as_deref(), Some("ASR worker started"));
    }

    #[test]
    fn draft_is_visible_during_processing_until_final_result_replaces_it() {
        let state = AppState::default();
        state.transition(AppPhase::Processing, Some("Transcribing locally.".into()));
        let draft = state.publish_result("local draft".into());
        assert_eq!(draft.phase, AppPhase::Processing);
        assert_eq!(draft.last_result.as_deref(), Some("local draft"));
        state.transition(
            AppPhase::Processing,
            Some("Correcting the transcript.".into()),
        );
        assert_eq!(state.snapshot().last_result.as_deref(), Some("local draft"));
        let final_state = state.complete("corrected".into(), "Done.".into());
        assert_eq!(final_state.last_result.as_deref(), Some("corrected"));
        assert_eq!(final_state.phase, AppPhase::Completed);
    }

    #[test]
    fn exactly_one_stop_claims_recording() {
        let lifecycle = PipelineLifecycle::default();
        let id = lifecycle.begin_start(PipelineMode::Translate).unwrap();
        lifecycle.mark_recording(id).unwrap();
        let (_, mode, _) = lifecycle.begin_processing().unwrap();
        assert_eq!(mode, PipelineMode::Translate);
        assert_eq!(
            lifecycle.begin_processing().unwrap_err(),
            "recording stop is already in progress"
        );
    }

    #[test]
    fn setup_test_identity_survives_processing_and_stale_cancel_cannot_touch_new_take() {
        let lifecycle = PipelineLifecycle::default();
        let test = lifecycle.begin_dictation_test().unwrap();
        assert!(lifecycle.is_dictation_test(test));
        lifecycle.mark_recording(test).unwrap();
        lifecycle.begin_processing_for(Some(test)).unwrap();
        assert!(lifecycle.is_dictation_test(test));
        lifecycle.finish(test);
        let normal = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
        assert_eq!(lifecycle.cancel_for(Some(test)).unwrap(), None);
        assert!(!lifecycle.is_cancelled(normal));
        assert!(!lifecycle.is_setup_test(normal));
        assert!(lifecycle.begin_processing_for(Some(test)).is_err());
        lifecycle.finish(normal);
        let (insertion, cancel) = lifecycle.begin_insertion_test().unwrap();
        assert!(lifecycle.is_setup_test(insertion));
        assert!(!lifecycle.is_dictation_test(insertion));
        assert!(lifecycle.begin_start(PipelineMode::Ask).is_err());
        lifecycle.cancel_for(Some(insertion)).unwrap();
        assert!(*cancel.borrow());
        let mut wrote = false;
        assert!(lifecycle
            .commit_side_effect(insertion, || {
                wrote = true;
                Ok::<_, String>(())
            })
            .is_err());
        assert!(!wrote);
    }

    #[test]
    fn edit_owns_the_pipeline_until_it_finishes() {
        let lifecycle = PipelineLifecycle::default();
        let id = lifecycle.begin_start(PipelineMode::Edit).unwrap();
        lifecycle.mark_recording(id).unwrap();
        let (_, mode, _) = lifecycle.begin_processing().unwrap();
        assert_eq!(mode, PipelineMode::Edit);
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_err());
        assert!(lifecycle.begin_start(PipelineMode::Translate).is_err());
        lifecycle.finish(id);
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_ok());
    }

    #[test]
    fn edit_cancel_during_start_prevents_recording_and_releases_ownership() {
        let lifecycle = PipelineLifecycle::default();
        let id = lifecycle.begin_start(PipelineMode::Edit).unwrap();
        assert_eq!(
            lifecycle.cancel().unwrap(),
            Some((id, PipelinePhase::Starting))
        );
        assert!(lifecycle.mark_recording(id).is_err());
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_err());
        lifecycle.finish(id);
        assert!(lifecycle.begin_start(PipelineMode::Translate).is_ok());
    }

    #[test]
    fn old_operation_cannot_clear_new_operation() {
        let lifecycle = PipelineLifecycle::default();
        let first = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
        lifecycle.finish(first);
        let second = lifecycle.begin_start(PipelineMode::Translate).unwrap();
        lifecycle.finish(first);
        assert_eq!(lifecycle.phase(), PipelinePhase::Starting);
        lifecycle.finish(second);
        assert_eq!(lifecycle.phase(), PipelinePhase::Idle);
    }

    #[test]
    fn cancelled_start_cannot_publish_recording() {
        let lifecycle = PipelineLifecycle::default();
        let id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
        lifecycle.cancel().unwrap();
        assert!(lifecycle.mark_recording(id).is_err());
        lifecycle.finish(id);
        assert_eq!(lifecycle.phase(), PipelinePhase::Idle);
    }

    fn start_processing_search(lifecycle: &PipelineLifecycle) -> (u64, watch::Receiver<bool>) {
        let id = lifecycle.begin_start(PipelineMode::Ask).unwrap();
        lifecycle.mark_recording(id).unwrap();
        let (_, _, cancel) = lifecycle.begin_processing().unwrap();
        (id, cancel)
    }

    #[test]
    fn search_cancel_before_launch_prevents_all_publication() {
        let lifecycle = PipelineLifecycle::default();
        let (id, _) = start_processing_search(&lifecycle);
        let state = AppState::default();
        lifecycle.cancel().unwrap();
        let mut launched = false;
        let result = lifecycle.commit_side_effect(id, || {
            launched = true;
            Ok::<(), ()>(())
        });
        if matches!(result, Ok(Ok(()))) {
            state.complete("query".into(), "opened".into());
        }
        assert!(result.is_err());
        assert!(!launched);
        assert_eq!(state.snapshot().phase, AppPhase::Idle);
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_err());
    }

    #[test]
    fn search_cancel_during_launch_observes_commit_and_completion_keeps_ownership() {
        use std::sync::{mpsc, Arc};

        let lifecycle = Arc::new(PipelineLifecycle::default());
        let state = Arc::new(AppState::default());
        let (id, cancel) = start_processing_search(&lifecycle);
        let (launch_started, wait_started) = mpsc::channel();
        let (release_launch, wait_release) = mpsc::channel();
        let launcher = {
            let lifecycle = lifecycle.clone();
            let state = state.clone();
            std::thread::spawn(move || {
                lifecycle
                    .commit_side_effect(id, || {
                        launch_started.send(()).unwrap();
                        wait_release.recv().unwrap();
                        Ok::<(), ()>(())
                    })
                    .unwrap()
                    .unwrap();
                // Represents History and Completed publication after launch.
                state.complete("query".into(), "opened".into());
            })
        };
        wait_started.recv().unwrap();
        let (cancel_requested, wait_cancel) = mpsc::channel();
        let canceller = {
            let lifecycle = lifecycle.clone();
            std::thread::spawn(move || {
                cancel_requested.send(()).unwrap();
                lifecycle.cancel().unwrap()
            })
        };
        wait_cancel.recv().unwrap();
        assert!(lifecycle.inner.try_lock().is_err());
        release_launch.send(()).unwrap();
        launcher.join().unwrap();
        assert_eq!(canceller.join().unwrap(), None);
        assert!(!*cancel.borrow());
        assert_eq!(lifecycle.cancel().unwrap(), None);
        assert_eq!(state.snapshot().phase, AppPhase::Completed);
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_err());
        assert!(lifecycle
            .commit_side_effect(id, || Ok::<(), ()>(()))
            .is_err());
        lifecycle.finish(id);
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_ok());
    }

    #[test]
    fn failed_search_launch_remains_cancellable() {
        let lifecycle = PipelineLifecycle::default();
        let (id, cancel) = start_processing_search(&lifecycle);
        assert_eq!(
            lifecycle
                .commit_side_effect(id, || Err::<(), _>("launch failed"))
                .unwrap(),
            Err("launch failed")
        );
        assert_eq!(
            lifecycle.cancel().unwrap(),
            Some((id, PipelinePhase::Processing))
        );
        assert!(*cancel.borrow());
    }

    #[test]
    fn stale_search_cannot_launch_under_a_new_operation() {
        let lifecycle = PipelineLifecycle::default();
        let (old_id, _) = start_processing_search(&lifecycle);
        lifecycle.finish(old_id);
        let (new_id, _) = start_processing_search(&lifecycle);
        let mut launched = false;
        assert!(lifecycle
            .commit_side_effect(old_id, || {
                launched = true;
                Ok::<(), ()>(())
            })
            .is_err());
        assert!(!launched);
        assert!(!lifecycle.is_cancelled(new_id));
        assert_eq!(
            lifecycle.cancel().unwrap(),
            Some((new_id, PipelinePhase::Processing))
        );
    }

    #[test]
    fn retry_claims_processing_and_releases_after_cancellation() {
        let lifecycle = PipelineLifecycle::default();
        let (id, cancel) = lifecycle.begin_non_recording(PipelineMode::Ask).unwrap();
        assert_eq!(lifecycle.phase(), PipelinePhase::Processing);
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_err());
        assert_eq!(
            lifecycle.cancel().unwrap(),
            Some((id, PipelinePhase::Processing))
        );
        assert!(*cancel.borrow());
        lifecycle.finish(id);
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_ok());
    }

    #[test]
    fn selected_text_translation_and_voice_modes_exclude_each_other() {
        let lifecycle = PipelineLifecycle::default();
        let (id, cancel) = lifecycle
            .begin_non_recording(PipelineMode::SelectedText)
            .unwrap();
        assert_eq!(lifecycle.phase(), PipelinePhase::Processing);
        assert_eq!(lifecycle.mode(), Some(PipelineMode::SelectedText));
        for mode in [
            PipelineMode::Dictate,
            PipelineMode::Translate,
            PipelineMode::Edit,
            PipelineMode::Ask,
        ] {
            assert!(lifecycle.begin_start(mode).is_err());
        }
        assert!(lifecycle
            .begin_non_recording(PipelineMode::SelectedText)
            .is_err());
        // It records nothing, so a stop cannot claim it.
        assert!(lifecycle.begin_processing().is_err());
        assert!(!*cancel.borrow());
        lifecycle.finish(id);

        let voice = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
        assert!(lifecycle
            .begin_non_recording(PipelineMode::SelectedText)
            .is_err());
        lifecycle.mark_recording(voice).unwrap();
        assert!(lifecycle
            .begin_non_recording(PipelineMode::SelectedText)
            .is_err());
        lifecycle.finish(voice);
        assert!(lifecycle
            .begin_non_recording(PipelineMode::SelectedText)
            .is_ok());
    }

    #[test]
    fn a_cancelled_selected_text_translation_cannot_replace_the_selection() {
        let lifecycle = PipelineLifecycle::default();
        let (id, cancel) = lifecycle
            .begin_non_recording(PipelineMode::SelectedText)
            .unwrap();
        assert_eq!(
            lifecycle.cancel().unwrap(),
            Some((id, PipelinePhase::Processing))
        );
        assert!(*cancel.borrow());
        assert!(lifecycle.is_cancelled(id));
        let mut replaced = false;
        assert!(lifecycle
            .commit_side_effect(id, || {
                replaced = true;
                Ok::<(), ()>(())
            })
            .is_err());
        assert!(!replaced);
        // The owner still holds the lifecycle until it publishes Idle.
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_err());
        lifecycle.finish(id);
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_ok());
    }

    #[test]
    fn a_committed_selected_text_replacement_ignores_a_later_cancel() {
        let lifecycle = PipelineLifecycle::default();
        let (id, cancel) = lifecycle
            .begin_non_recording(PipelineMode::SelectedText)
            .unwrap();
        assert!(lifecycle
            .commit_side_effect(id, || Ok::<_, std::convert::Infallible>(()))
            .unwrap()
            .is_ok());
        assert_eq!(lifecycle.cancel().unwrap(), None);
        assert!(!*cancel.borrow());
        assert!(!lifecycle.is_cancelled(id));
    }

    #[test]
    fn retry_commit_rejects_prior_cancellation() {
        let lifecycle = PipelineLifecycle::default();
        let (id, _) = lifecycle
            .begin_non_recording(PipelineMode::Dictate)
            .unwrap();
        lifecycle.cancel().unwrap();
        assert!(lifecycle
            .commit_side_effect(id, || Ok::<(), ()>(()))
            .is_err());
    }

    #[test]
    fn retry_commit_seals_the_operation_before_a_waiting_cancel_can_win() {
        let lifecycle = PipelineLifecycle::default();
        let (id, cancel) = lifecycle
            .begin_non_recording(PipelineMode::Dictate)
            .unwrap();
        assert!(lifecycle
            .commit_side_effect(id, || Ok::<(), ()>(()))
            .unwrap()
            .is_ok());
        assert_eq!(lifecycle.cancel().unwrap(), None);
        assert!(!*cancel.borrow());
        assert!(!lifecycle.is_cancelled(id));
        assert!(lifecycle
            .commit_side_effect(id, || Ok::<(), ()>(()))
            .is_err());
    }

    #[test]
    fn committed_retry_keeps_the_lifecycle_until_completion_is_published() {
        let lifecycle = PipelineLifecycle::default();
        let (id, _) = lifecycle.begin_non_recording(PipelineMode::Ask).unwrap();
        assert!(lifecycle
            .commit_side_effect(id, || Ok::<(), ()>(()))
            .unwrap()
            .is_ok());

        // Completion is published between commit and finish; no recording
        // may start in that window and later be overwritten by it.
        assert_eq!(lifecycle.phase(), PipelinePhase::Processing);
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_err());
        assert!(lifecycle
            .begin_non_recording(PipelineMode::Dictate)
            .is_err());
        lifecycle.finish(id);
        assert_eq!(lifecycle.phase(), PipelinePhase::Idle);
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_ok());
    }

    #[test]
    fn phase_observers_see_every_lifecycle_transition() {
        let lifecycle = PipelineLifecycle::default();
        let mut phases = lifecycle.subscribe_phase();
        assert!(phases.has_changed().unwrap());
        assert_eq!(*phases.borrow_and_update(), PipelinePhase::Idle);
        let id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
        assert_eq!(*phases.borrow_and_update(), PipelinePhase::Starting);
        lifecycle.mark_recording(id).unwrap();
        assert_eq!(*phases.borrow_and_update(), PipelinePhase::Recording);
        lifecycle.begin_processing().unwrap();
        assert_eq!(*phases.borrow_and_update(), PipelinePhase::Processing);
        // A stale finish changes nothing and is not reported.
        lifecycle.finish(id + 1);
        assert!(!phases.has_changed().unwrap());
        lifecycle.finish(id);
        assert_eq!(*phases.borrow_and_update(), PipelinePhase::Idle);
        let (retry, _) = lifecycle.begin_non_recording(PipelineMode::Ask).unwrap();
        assert_eq!(*phases.borrow_and_update(), PipelinePhase::Processing);
        lifecycle.finish(retry);
        assert_eq!(*phases.borrow_and_update(), PipelinePhase::Idle);
    }

    #[test]
    fn only_a_busy_phase_needs_a_terminal_phase_on_exit() {
        for busy in [
            AppPhase::Recording,
            AppPhase::Processing,
            AppPhase::Injecting,
        ] {
            assert_eq!(
                unfinished_exit_phase(&busy, false, "Dictation cancelled."),
                Some((AppPhase::Error, OPERATION_INCOMPLETE))
            );
            assert_eq!(
                unfinished_exit_phase(&busy, true, "Dictation cancelled."),
                Some((AppPhase::Idle, "Dictation cancelled."))
            );
        }
        for terminal in [AppPhase::Idle, AppPhase::Completed, AppPhase::Error] {
            assert_eq!(unfinished_exit_phase(&terminal, false, "x"), None);
            assert_eq!(unfinished_exit_phase(&terminal, true, "x"), None);
        }
    }

    /// Mirrors `stop_recording_for`: the phase guard is declared after the
    /// lifecycle guard, so it publishes while the operation is still owned.
    fn exit_early(
        lifecycle: &PipelineLifecycle,
        state: &AppState,
        cancel: bool,
    ) -> Result<(), &'static str> {
        let (id, _, _) = lifecycle.begin_processing()?;
        struct Finish<'a>(&'a PipelineLifecycle, u64);
        impl Drop for Finish<'_> {
            fn drop(&mut self) {
                self.0.finish(self.1);
            }
        }
        let _owner = Finish(lifecycle, id);
        let _phase = TerminalPhaseGuard::new(
            state,
            lifecycle,
            id,
            "Dictation cancelled.",
            |phase, message| {
                assert_eq!(lifecycle.phase(), PipelinePhase::Processing);
                state.transition(phase, Some(message.into()));
            },
        );
        state.transition(AppPhase::Processing, Some("Transcribing locally.".into()));
        if cancel {
            lifecycle.cancel()?;
        }
        Err("session lock poisoned")
    }

    fn recording(lifecycle: &PipelineLifecycle, state: &AppState) {
        let id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
        lifecycle.mark_recording(id).unwrap();
        state.transition(AppPhase::Recording, None);
    }

    #[test]
    fn an_early_stop_exit_publishes_error_before_releasing_the_lifecycle() {
        let lifecycle = PipelineLifecycle::default();
        let state = AppState::default();
        recording(&lifecycle, &state);
        assert!(exit_early(&lifecycle, &state, false).is_err());
        let snapshot = state.snapshot();
        assert_eq!(snapshot.phase, AppPhase::Error);
        assert_eq!(snapshot.message.as_deref(), Some(OPERATION_INCOMPLETE));
        assert_eq!(lifecycle.phase(), PipelinePhase::Idle);
    }

    #[test]
    fn an_early_exit_after_cancel_publishes_idle() {
        let lifecycle = PipelineLifecycle::default();
        let state = AppState::default();
        recording(&lifecycle, &state);
        assert!(exit_early(&lifecycle, &state, true).is_err());
        let snapshot = state.snapshot();
        assert_eq!(snapshot.phase, AppPhase::Idle);
        assert_eq!(snapshot.message.as_deref(), Some("Dictation cancelled."));
    }

    #[test]
    fn a_published_outcome_is_not_replaced_on_exit() {
        let lifecycle = PipelineLifecycle::default();
        let state = AppState::default();
        let id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
        {
            let _phase = TerminalPhaseGuard::new(&state, &lifecycle, id, "x", |_, _| {
                panic!("a terminal phase must not be republished")
            });
            state.complete("text".into(), "Dictation inserted successfully.".into());
        }
        assert_eq!(state.snapshot().phase, AppPhase::Completed);
    }

    #[test]
    fn failed_retry_commit_stays_cancellable() {
        let lifecycle = PipelineLifecycle::default();
        let (id, _) = lifecycle
            .begin_non_recording(PipelineMode::Dictate)
            .unwrap();
        assert!(lifecycle
            .commit_side_effect(id, || Err::<(), ()>(()))
            .unwrap()
            .is_err());
        assert_eq!(
            lifecycle.cancel().unwrap(),
            Some((id, PipelinePhase::Processing))
        );
    }
}
