use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

pub(crate) const WINDOW_LABEL: &str = "ask-answer";

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct AnswerPayload {
    #[serde(rename = "operationId")]
    pub operation_id: u64,
    pub payload: String,
}

/// Backend-owned, monotonic panel state.  A stale completion/dismiss cannot
/// overwrite a newer operation, and cancellation never clears an answer that
/// was already completed.
#[derive(Default)]
pub(crate) struct AnswerPanelState {
    current: Mutex<Option<AnswerPayload>>,
}
impl AnswerPanelState {
    pub(crate) fn publish(&self, operation_id: u64, payload: String) -> bool {
        let Ok(mut current) = self.current.lock() else {
            return false;
        };
        if current
            .as_ref()
            .is_some_and(|value| value.operation_id > operation_id)
        {
            return false;
        }
        *current = Some(AnswerPayload {
            operation_id,
            payload,
        });
        true
    }
    pub(crate) fn dismiss(&self, operation_id: u64) -> bool {
        let Ok(mut current) = self.current.lock() else {
            return false;
        };
        if current
            .as_ref()
            .is_some_and(|value| value.operation_id == operation_id)
        {
            *current = None;
            true
        } else {
            false
        }
    }
    pub(crate) fn current(&self) -> Option<AnswerPayload> {
        self.current.lock().ok()?.clone()
    }
}

pub(crate) fn create(app: &AppHandle) -> tauri::Result<()> {
    WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("index.html".into()))
        .title("Ask answer")
        .inner_size(500.0, 330.0)
        .min_inner_size(320.0, 180.0)
        .always_on_top(true)
        .visible(false)
        .build()?;
    Ok(())
}

pub(crate) fn show(app: &AppHandle, operation_id: u64, payload: &str) -> bool {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return false;
    };
    if app
        .emit(
            "ask-answer",
            serde_json::json!({"operationId": operation_id, "payload": payload }),
        )
        .is_err()
    {
        return false;
    }
    // `show` intentionally avoids set_focus: a spoken answer must not steal
    // the foreground target. Pointer interaction is enabled by the window.
    window.show().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_updates_and_dismisses_cannot_clobber_latest_answer() {
        let state = AnswerPanelState::default();
        assert!(state.publish(2, "new".into()));
        assert!(!state.publish(1, "old".into()));
        assert!(!state.dismiss(1));
        assert_eq!(state.current().unwrap().payload, "new");
        assert!(state.dismiss(2));
    }

    #[test]
    fn payload_uses_frontend_operation_id_shape() {
        let json = serde_json::to_value(AnswerPayload {
            operation_id: 7,
            payload: "answer".into(),
        })
        .unwrap();
        assert_eq!(json["operationId"], 7);
        assert!(json.get("operation_id").is_none());
    }
}
