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

struct PipelineOperation {
    id: u64,
    phase: PipelinePhase,
    cancel: watch::Sender<bool>,
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
    pub fn begin_start(&self) -> Result<u64, &'static str> {
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
            phase: PipelinePhase::Starting,
            cancel,
        });
        Ok(id)
    }

    pub fn mark_recording(&self, id: u64) -> Result<(), &'static str> {
        self.transition(id, PipelinePhase::Starting, PipelinePhase::Recording)
    }

    pub fn begin_processing(&self) -> Result<(u64, watch::Receiver<bool>), &'static str> {
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
        Ok((operation.id, operation.cancel.subscribe()))
    }

    pub fn cancel(&self) -> Result<Option<(u64, PipelinePhase)>, &'static str> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| "pipeline lifecycle is unavailable")?;
        let Some(operation) = inner.as_ref() else {
            return Ok(None);
        };
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
    fn exactly_one_stop_claims_recording() {
        let lifecycle = PipelineLifecycle::default();
        let id = lifecycle.begin_start().unwrap();
        lifecycle.mark_recording(id).unwrap();
        assert!(lifecycle.begin_processing().is_ok());
        assert_eq!(
            lifecycle.begin_processing().unwrap_err(),
            "recording stop is already in progress"
        );
    }

    #[test]
    fn old_operation_cannot_clear_new_operation() {
        let lifecycle = PipelineLifecycle::default();
        let first = lifecycle.begin_start().unwrap();
        lifecycle.finish(first);
        let second = lifecycle.begin_start().unwrap();
        lifecycle.finish(first);
        assert_eq!(lifecycle.phase(), PipelinePhase::Starting);
        lifecycle.finish(second);
        assert_eq!(lifecycle.phase(), PipelinePhase::Idle);
    }

    #[test]
    fn cancelled_start_cannot_publish_recording() {
        let lifecycle = PipelineLifecycle::default();
        let id = lifecycle.begin_start().unwrap();
        lifecycle.cancel().unwrap();
        assert!(lifecycle.mark_recording(id).is_err());
        lifecycle.finish(id);
        assert_eq!(lifecycle.phase(), PipelinePhase::Idle);
    }
}
