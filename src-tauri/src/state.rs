use std::sync::{Mutex, RwLock};

use chrono::Utc;
use tokio::sync::watch;

use crate::types::{AppPhase, AppStateSnapshot};

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
}

struct PipelineOperation {
    id: u64,
    mode: PipelineMode,
    phase: PipelinePhase,
    cancel: watch::Sender<bool>,
    /// Successful side effects seal cancellation until the owner finishes.
    committed: bool,
}

pub struct PipelineLifecycle {
    inner: Mutex<Option<PipelineOperation>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl Default for PipelineLifecycle {
    fn default() -> Self {
        Self {
            inner: Mutex::new(None),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

impl PipelineLifecycle {
    pub fn begin_start(&self, mode: PipelineMode) -> Result<u64, &'static str> {
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
        });
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
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "pipeline lifecycle is unavailable")?;
        let operation = inner.as_mut().ok_or("no recording is active")?;
        if operation.phase != PipelinePhase::Recording {
            return Err("recording stop is already in progress");
        }
        if *operation.cancel.borrow() {
            return Err("pipeline operation was cancelled");
        }
        operation.phase = PipelinePhase::Processing;
        Ok((operation.id, operation.mode, operation.cancel.subscribe()))
    }

    pub fn cancel(&self) -> Result<Option<(u64, PipelinePhase)>, &'static str> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| "pipeline lifecycle is unavailable")?;
        let Some(operation) = inner.as_ref() else {
            return Ok(None);
        };
        if operation.committed || *operation.cancel.borrow() {
            return Ok(None);
        }
        operation.cancel.send_replace(true);
        Ok(Some((operation.id, operation.phase)))
    }

    pub fn finish(&self, id: u64) {
        if let Ok(mut inner) = self.inner.lock() {
            if inner.as_ref().is_some_and(|operation| operation.id == id) {
                *inner = None;
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

    /// Serialize a synchronous side effect with cancellation. On success the
    /// operation stays owned and cannot be cancelled until completion is
    /// published and the owner calls `finish`. On failure it stays cancellable.
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
                message: None,
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

    pub fn transition(&self, phase: AppPhase, message: Option<String>) -> AppStateSnapshot {
        let mut state = self.inner.write().expect("app state lock poisoned");
        state.phase = phase;
        state.message = message;
        state.updated_at = Utc::now().to_rfc3339();
        state.clone()
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
        state.message = Some(message);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_idle_without_sensitive_message() {
        let state = AppState::default().snapshot();
        assert_eq!(state.phase, AppPhase::Idle);
        assert_eq!(state.message, None);
        assert_eq!(state.last_result, None);
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
}
