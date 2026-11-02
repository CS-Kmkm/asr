use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::watch;

pub(crate) const WINDOW_LABEL: &str = "ask-answer";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AnswerKind {
    Generated,
    Notice,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct AnswerPayload {
    #[serde(rename = "operationId")]
    pub operation_id: u64,
    pub payload: String,
    pub kind: AnswerKind,
}

/// Backend-owned, monotonic panel state.  A stale completion/dismiss cannot
/// overwrite a newer operation, and cancellation never clears an answer that
/// was already completed. An in-progress generation preview also owns the
/// panel: an older completion or dismiss cannot replace or hide it.
#[derive(Default)]
pub(crate) struct AnswerPanelState {
    current: Mutex<PanelInner>,
}

#[derive(Default)]
struct PanelInner {
    answer: Option<AnswerPayload>,
    preview: Option<u64>,
}

impl PanelInner {
    fn newest(&self) -> Option<u64> {
        self.answer
            .as_ref()
            .map(|value| value.operation_id)
            .max(self.preview)
    }
}

impl AnswerPanelState {
    pub(crate) fn publish(&self, operation_id: u64, payload: String) -> bool {
        self.publish_kind(operation_id, payload, AnswerKind::Generated)
    }
    pub(crate) fn publish_notice(&self, operation_id: u64, payload: String) -> bool {
        self.publish_kind(operation_id, payload, AnswerKind::Notice)
    }
    fn publish_kind(&self, operation_id: u64, payload: String, kind: AnswerKind) -> bool {
        let Ok(mut current) = self.current.lock() else {
            return false;
        };
        if current.newest().is_some_and(|newest| newest > operation_id) {
            return false;
        }
        // The validated answer replaces this operation's preview.
        current.preview = None;
        current.answer = Some(AnswerPayload {
            operation_id,
            payload,
            kind,
        });
        true
    }
    /// Claims the panel for a live preview of `operation_id`. Fails when the
    /// operation already completed or a newer operation owns the panel.
    fn begin_preview(&self, operation_id: u64) -> bool {
        let Ok(mut current) = self.current.lock() else {
            return false;
        };
        let completed = current
            .answer
            .as_ref()
            .is_some_and(|value| value.operation_id >= operation_id);
        if completed
            || current
                .preview
                .is_some_and(|preview| preview > operation_id)
        {
            return false;
        }
        current.preview = Some(operation_id);
        true
    }
    /// Ends a preview that no final answer replaced. `clear` runs inside the
    /// critical section so it cannot withdraw a newer preview or answer.
    fn end_preview_with(&self, operation_id: u64, clear: impl FnOnce()) -> bool {
        let Ok(mut current) = self.current.lock() else {
            return false;
        };
        if current.preview != Some(operation_id) {
            return false;
        }
        current.preview = None;
        clear();
        true
    }
    #[cfg(test)]
    pub(crate) fn dismiss(&self, operation_id: u64) -> bool {
        self.dismiss_with(operation_id, || true)
    }
    /// Keep the ownership check and window hide in one critical section so an
    /// older dismiss cannot hide a newly published answer or a newer preview.
    pub(crate) fn dismiss_with(&self, operation_id: u64, hide: impl FnOnce() -> bool) -> bool {
        let Ok(mut current) = self.current.lock() else {
            return false;
        };
        if current
            .answer
            .as_ref()
            .is_some_and(|value| value.operation_id == operation_id)
            && !current
                .preview
                .is_some_and(|preview| preview > operation_id)
            && hide()
        {
            current.answer = None;
            true
        } else {
            false
        }
    }
    pub(crate) fn current(&self) -> Option<AnswerPayload> {
        self.current.lock().ok()?.answer.clone()
    }
}

/// Minimum spacing between preview events; the final answer always follows.
pub(crate) const PREVIEW_INTERVAL: Duration = Duration::from_millis(100);
pub(crate) const PREVIEW_EVENT: &str = "ask-answer-preview";

/// Unvalidated, in-progress generation text shown only in the answer panel.
/// `active: false` withdraws the preview of that operation.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AnswerPreview<'a> {
    pub operation_id: u64,
    pub text: &'a str,
    pub active: bool,
}

/// The answer panel window operations the preview needs, so the throttle and
/// ownership rules are testable without a webview.
pub(crate) trait PreviewWindow {
    fn is_visible(&self) -> bool;
    fn show(&self) -> bool;
    fn hide(&self) -> bool;
    fn send(&self, preview: &AnswerPreview<'_>) -> bool;
}

pub(crate) struct AppPreviewWindow<'a>(pub &'a AppHandle);

impl PreviewWindow for AppPreviewWindow<'_> {
    fn is_visible(&self) -> bool {
        self.0
            .get_webview_window(WINDOW_LABEL)
            .and_then(|window| window.is_visible().ok())
            .unwrap_or(false)
    }
    fn show(&self) -> bool {
        // Like the final answer, the preview must not steal the foreground.
        self.0
            .get_webview_window(WINDOW_LABEL)
            .is_some_and(|window| window.show().is_ok())
    }
    fn hide(&self) -> bool {
        self.0
            .get_webview_window(WINDOW_LABEL)
            .is_some_and(|window| window.hide().is_ok())
    }
    fn send(&self, preview: &AnswerPreview<'_>) -> bool {
        // Generated text goes to the answer panel window only: never
        // broadcast or log it.
        self.0.emit_to(WINDOW_LABEL, PREVIEW_EVENT, preview).is_ok()
    }
}

/// Forwards one operation's streamed generation to the answer panel as a
/// throttled preview of the accumulated text. Dropping it without a final
/// answer for the operation (failure, cancellation, clipboard fallback)
/// withdraws the preview and hides a panel that the preview itself opened.
pub(crate) struct LivePreview<'a, W: PreviewWindow> {
    window: W,
    state: &'a AnswerPanelState,
    operation_id: u64,
    cancel: watch::Receiver<bool>,
    interval: Duration,
    text: String,
    last_sent: Option<Instant>,
    started: bool,
    stopped: bool,
    hide_on_end: bool,
}

impl<'a, W: PreviewWindow> LivePreview<'a, W> {
    pub(crate) fn new(
        window: W,
        state: &'a AnswerPanelState,
        operation_id: u64,
        cancel: watch::Receiver<bool>,
    ) -> Self {
        Self {
            window,
            state,
            operation_id,
            cancel,
            interval: PREVIEW_INTERVAL,
            text: String::new(),
            last_sent: None,
            started: false,
            stopped: false,
            hide_on_end: false,
        }
    }

    pub(crate) fn push(&mut self, delta: &str) {
        self.push_at(delta, Instant::now());
    }

    fn push_at(&mut self, delta: &str, now: Instant) {
        if self.stopped {
            return;
        }
        if *self.cancel.borrow() {
            self.stopped = true;
            return;
        }
        self.text.push_str(delta);
        if self.text.trim().is_empty()
            || self
                .last_sent
                .is_some_and(|last| now.saturating_duration_since(last) < self.interval)
        {
            return;
        }
        if !self.started {
            if !self.state.begin_preview(self.operation_id) {
                self.stopped = true;
                return;
            }
            self.started = true;
            self.hide_on_end = !self.window.is_visible();
        }
        let sent = self.window.send(&AnswerPreview {
            operation_id: self.operation_id,
            text: &self.text,
            active: true,
        });
        if sent {
            if self.last_sent.is_none() {
                self.window.show();
            }
            self.last_sent = Some(now);
        }
    }
}

impl<W: PreviewWindow> Drop for LivePreview<'_, W> {
    fn drop(&mut self) {
        if !self.started {
            return;
        }
        let window = &self.window;
        let operation_id = self.operation_id;
        let hide_on_end = self.hide_on_end;
        self.state.end_preview_with(operation_id, || {
            window.send(&AnswerPreview {
                operation_id,
                text: "",
                active: false,
            });
            if hide_on_end {
                window.hide();
            }
        });
    }
}

pub(crate) fn create(app: &AppHandle) -> tauri::Result<()> {
    WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("index.html".into()))
        .title("Ask answer")
        .inner_size(500.0, 330.0)
        .min_inner_size(320.0, 180.0)
        .always_on_top(true)
        .focused(false)
        .visible(false)
        .build()?;
    Ok(())
}

pub(crate) fn show(app: &AppHandle, operation_id: u64, payload: &str) -> bool {
    show_kind(app, operation_id, payload, AnswerKind::Generated)
}

pub(crate) fn show_notice(app: &AppHandle, operation_id: u64, payload: &str) -> bool {
    show_kind(app, operation_id, payload, AnswerKind::Notice)
}

fn show_kind(app: &AppHandle, operation_id: u64, payload: &str, kind: AnswerKind) -> bool {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return false;
    };
    if app
        .emit(
            "ask-answer",
            serde_json::json!({"operationId": operation_id, "payload": payload, "kind": kind }),
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
    fn failed_hide_keeps_the_answer_and_stale_dismiss_does_not_hide() {
        let state = AnswerPanelState::default();
        assert!(state.publish(1, "first".into()));
        assert!(!state.dismiss_with(1, || false));
        assert_eq!(state.current().unwrap().operation_id, 1);
        assert!(state.publish(2, "new".into()));
        assert!(!state.dismiss_with(1, || panic!("stale hide was called")));
        assert_eq!(state.current().unwrap().operation_id, 2);
    }

    #[derive(Default)]
    struct FakeWindow {
        visible: std::cell::Cell<bool>,
        sent: std::cell::RefCell<Vec<(u64, String, bool)>>,
    }

    impl PreviewWindow for &FakeWindow {
        fn is_visible(&self) -> bool {
            self.visible.get()
        }
        fn show(&self) -> bool {
            self.visible.set(true);
            true
        }
        fn hide(&self) -> bool {
            self.visible.set(false);
            true
        }
        fn send(&self, preview: &AnswerPreview<'_>) -> bool {
            self.sent.borrow_mut().push((
                preview.operation_id,
                preview.text.to_owned(),
                preview.active,
            ));
            true
        }
    }

    fn not_cancelled() -> watch::Receiver<bool> {
        watch::channel(false).1
    }

    #[test]
    fn preview_sends_throttled_accumulated_text_and_final_answer_replaces_it() {
        let state = AnswerPanelState::default();
        let window = FakeWindow::default();
        let start = Instant::now();
        {
            let mut preview = LivePreview::new(&window, &state, 3, not_cancelled());
            preview.push_at("  ", start);
            assert!(window.sent.borrow().is_empty() && !window.visible.get());
            preview.push_at("Hello", start);
            preview.push_at(" wor", start + Duration::from_millis(40));
            preview.push_at("ld", start + PREVIEW_INTERVAL);
            assert!(window.visible.get());
            assert_eq!(
                *window.sent.borrow(),
                [
                    (3, "  Hello".to_owned(), true),
                    (3, "  Hello world".to_owned(), true)
                ]
            );
            // An older operation can neither replace nor hide the preview.
            assert!(!state.publish(2, "old".into()));
            assert!(state.publish(3, "Hello world".into()));
        }
        // The final answer owns the panel, so dropping the preview is a no-op.
        assert_eq!(window.sent.borrow().len(), 2);
        assert!(window.visible.get());
        assert_eq!(state.current().unwrap().payload, "Hello world");
    }

    #[test]
    fn unfinished_preview_is_withdrawn_and_restores_the_hidden_panel() {
        let state = AnswerPanelState::default();
        let window = FakeWindow::default();
        drop({
            let mut preview = LivePreview::new(&window, &state, 4, not_cancelled());
            preview.push("partial");
            preview
        });
        assert_eq!(
            *window.sent.borrow(),
            [(4, "partial".into(), true), (4, String::new(), false)]
        );
        assert!(!window.visible.get());
        assert!(state.current().is_none());

        // A panel that was already showing an answer stays visible.
        assert!(state.publish(5, "earlier answer".into()));
        window.visible.set(true);
        drop({
            let mut preview = LivePreview::new(&window, &state, 6, not_cancelled());
            preview.push("partial");
            assert!(!state.dismiss(5), "an older dismiss hid the preview");
            preview
        });
        assert!(window.visible.get());
        assert_eq!(state.current().unwrap().payload, "earlier answer");
    }

    #[test]
    fn cancelled_or_stale_operations_never_reach_the_panel() {
        let state = AnswerPanelState::default();
        let window = FakeWindow::default();
        let (cancel_tx, cancel) = watch::channel(false);
        cancel_tx.send(true).unwrap();
        drop({
            let mut preview = LivePreview::new(&window, &state, 7, cancel);
            preview.push("cancelled text");
            preview
        });
        assert!(state.publish(9, "newer".into()));
        drop({
            let mut preview = LivePreview::new(&window, &state, 8, not_cancelled());
            preview.push("stale text");
            preview
        });
        assert!(window.sent.borrow().is_empty());
        assert!(!window.visible.get());
        assert_eq!(state.current().unwrap().operation_id, 9);
    }

    #[test]
    fn preview_payload_uses_frontend_shape() {
        let json = serde_json::to_value(AnswerPreview {
            operation_id: 7,
            text: "partial",
            active: true,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"operationId": 7, "text": "partial", "active": true})
        );
    }

    #[test]
    fn payload_uses_frontend_operation_id_shape() {
        let json = serde_json::to_value(AnswerPayload {
            operation_id: 7,
            payload: "answer".into(),
            kind: AnswerKind::Generated,
        })
        .unwrap();
        assert_eq!(json["operationId"], 7);
        assert!(json.get("operation_id").is_none());
    }
}
