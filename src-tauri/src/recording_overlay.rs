use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};

use crate::types::AppPhase;

const WINDOW_LABEL: &str = "recording-overlay";
const RECORDING_WIDTH: f64 = 172.0;
const TRANSLATION_WIDTH: f64 = 286.0;
const RECORDING_HEIGHT: f64 = 48.0;
const NOTICE_WIDTH: f64 = 380.0;
const NOTICE_HEIGHT: f64 = 64.0;
const NOTICE_DURATION: Duration = Duration::from_secs(5);
/// Failure and warning sentences are longer than insertion outcomes; the
/// overlay wraps them over up to three lines.
const MESSAGE_NOTICE_WIDTH: f64 = 420.0;
const MESSAGE_NOTICE_HEIGHT: f64 = 84.0;
const BOTTOM_MARGIN: f64 = 24.0;

/// Advanced by every phase change and notice. A notice timer hides the window
/// only if nothing replaced its notice, so it never hides a later recording.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn create(app: &AppHandle) -> tauri::Result<()> {
    let window = WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("index.html".into()))
        .title("Recording")
        .inner_size(RECORDING_WIDTH, RECORDING_HEIGHT)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .closable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focusable(false)
        .visible(false)
        .build()?;

    // Dictate mode remains click-through. Voice Translate temporarily enables
    // pointer events for its target-language control without activating this window.
    window.set_ignore_cursor_events(true)?;
    position_on_primary_monitor(&window)
}

/// How the overlay presents a published phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PhaseView {
    /// The live recording or processing view.
    Live,
    /// The phase's message as a notice: an Error must not just disappear,
    /// since the overlay is often the only visible surface.
    Notice(NoticeSeverity),
    Hidden,
}

pub(crate) fn phase_view(phase: &AppPhase) -> PhaseView {
    match phase {
        AppPhase::Recording | AppPhase::Processing | AppPhase::Injecting => PhaseView::Live,
        AppPhase::Error => PhaseView::Notice(NoticeSeverity::Failure),
        AppPhase::Idle | AppPhase::Completed => PhaseView::Hidden,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NoticeSeverity {
    /// The operation failed; nothing was inserted.
    Failure,
    /// The operation completed with a degraded result.
    Warning,
}

/// How long a message notice stays up. Failures explain what to do next, so
/// they stay longer than an insertion outcome.
pub(crate) fn notice_duration(severity: NoticeSeverity) -> Duration {
    match severity {
        NoticeSeverity::Failure => Duration::from_secs(8),
        NoticeSeverity::Warning => Duration::from_secs(6),
    }
}

/// Presents a published phase and its message (see `phase_view`).
pub(crate) fn publish(app: &AppHandle, phase: &AppPhase, message: &str) {
    match phase_view(phase) {
        PhaseView::Notice(severity) => show_message_notice(app, severity, message),
        PhaseView::Live | PhaseView::Hidden => set_phase(app, phase),
    }
}

pub(crate) fn set_phase(app: &AppHandle, phase: &AppPhase) {
    GENERATION.fetch_add(1, Ordering::AcqRel);
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };

    if phase_view(phase) == PhaseView::Live {
        let _ = window.set_size(LogicalSize::new(RECORDING_WIDTH, RECORDING_HEIGHT));
        // Recalculate whenever shown in case display layout, DPI, or the
        // recording/processing window size changed.
        let _ = position_on_primary_monitor(&window);
        let _ = window.show();
    } else {
        let _ = window.hide();
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct InsertionNotice<'a> {
    outcome: &'a str,
    detail: Option<&'a str>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MessageNotice<'a> {
    severity: NoticeSeverity,
    /// A fixed backend sentence; the overlay localizes known ones.
    message: &'a str,
    duration_ms: u64,
}

/// Shows a non-confirmed insertion outcome for a few seconds. The overlay is
/// click-through and never focusable, so the notice cannot take the target's
/// focus. Payloads are fixed codes; the overlay localizes them.
pub(crate) fn show_insertion_notice(app: &AppHandle, outcome: &str, detail: Option<&str>) {
    show_notice(app, (NOTICE_WIDTH, NOTICE_HEIGHT), NOTICE_DURATION, |app| {
        let _ = app.emit_to(
            WINDOW_LABEL,
            "insertion-notice",
            InsertionNotice { outcome, detail },
        );
    });
}

/// Shows a failure or warning sentence for `notice_duration`, so hotkey
/// workflows report their outcome while the main window stays hidden.
pub(crate) fn show_message_notice(app: &AppHandle, severity: NoticeSeverity, message: &str) {
    let duration = notice_duration(severity);
    show_notice(
        app,
        (MESSAGE_NOTICE_WIDTH, MESSAGE_NOTICE_HEIGHT),
        duration,
        |app| {
            let _ = app.emit_to(
                WINDOW_LABEL,
                "overlay-message",
                MessageNotice {
                    severity,
                    message,
                    duration_ms: duration.as_millis() as u64,
                },
            );
        },
    );
}

fn show_notice(
    app: &AppHandle,
    (width, height): (f64, f64),
    duration: Duration,
    emit: impl FnOnce(&AppHandle),
) {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    emit(app);
    let _ = window.set_ignore_cursor_events(true);
    let _ = window.set_size(LogicalSize::new(width, height));
    let _ = position_on_primary_monitor(&window);
    let _ = window.show();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(duration).await;
        let handle = app.clone();
        // Window commands run in order on the main thread. Checking there
        // means a phase change either precedes this check (no hide) or its
        // show is queued after this hide.
        let _ = app.run_on_main_thread(move || {
            if GENERATION.load(Ordering::Acquire) == generation {
                if let Some(window) = handle.get_webview_window(WINDOW_LABEL) {
                    let _ = window.hide();
                }
            }
        });
    });
}

pub(crate) fn set_interactive(app: &AppHandle, interactive: bool) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.set_ignore_cursor_events(!interactive);
        let width = if interactive {
            TRANSLATION_WIDTH
        } else {
            RECORDING_WIDTH
        };
        let _ = window.set_size(LogicalSize::new(width, RECORDING_HEIGHT));
        let _ = position_on_primary_monitor(&window);
    }
}

fn position_on_primary_monitor(window: &WebviewWindow) -> tauri::Result<()> {
    let Some(monitor) = window.primary_monitor()? else {
        return Ok(());
    };
    let work_area = monitor.work_area();
    let window_size = window.outer_size()?;
    let bottom_margin = (BOTTOM_MARGIN * monitor.scale_factor()).round() as i32;
    let x =
        work_area.position.x + (work_area.size.width.saturating_sub(window_size.width) / 2) as i32;
    let y = work_area.position.y + work_area.size.height as i32
        - window_size.height as i32
        - bottom_margin;
    window.set_position(PhysicalPosition::new(x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_phase_is_shown_as_a_failure_notice_instead_of_hiding() {
        assert_eq!(
            phase_view(&AppPhase::Error),
            PhaseView::Notice(NoticeSeverity::Failure)
        );
        for live in [
            AppPhase::Recording,
            AppPhase::Processing,
            AppPhase::Injecting,
        ] {
            assert_eq!(phase_view(&live), PhaseView::Live);
        }
        // Success and cancellation need no notice: the text appeared, or the
        // user asked for nothing to happen.
        assert_eq!(phase_view(&AppPhase::Completed), PhaseView::Hidden);
        assert_eq!(phase_view(&AppPhase::Idle), PhaseView::Hidden);
    }

    #[test]
    fn failures_stay_up_longer_than_warnings_and_insertion_outcomes() {
        assert_eq!(
            notice_duration(NoticeSeverity::Failure),
            Duration::from_secs(8)
        );
        assert_eq!(
            notice_duration(NoticeSeverity::Warning),
            Duration::from_secs(6)
        );
        assert!(notice_duration(NoticeSeverity::Warning) > NOTICE_DURATION);
    }
}
