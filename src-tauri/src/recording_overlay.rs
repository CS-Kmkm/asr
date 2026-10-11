use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};

use crate::types::AppPhase;

const WINDOW_LABEL: &str = "recording-overlay";
/// Wide enough for the label, the waveform, and the cancel button.
const RECORDING_WIDTH: f64 = 200.0;
/// Adds Voice Translate's target-language control.
const TRANSLATION_WIDTH: f64 = 314.0;
const RECORDING_HEIGHT: f64 = 48.0;
const NOTICE_WIDTH: f64 = 380.0;
const NOTICE_HEIGHT: f64 = 64.0;
const NOTICE_DURATION: Duration = Duration::from_secs(5);
/// Failure and warning sentences are longer than insertion outcomes; the
/// overlay wraps them over up to three lines.
const MESSAGE_NOTICE_WIDTH: f64 = 420.0;
const MESSAGE_NOTICE_HEIGHT: f64 = 84.0;
const BOTTOM_MARGIN: f64 = 24.0;
/// A refused hotkey press is flashed inside the live view this long.
const HINT_DURATION: Duration = Duration::from_secs(3);

/// Advanced by every phase change and notice. A notice timer hides the window
/// only if nothing replaced its notice, so it never hides a later recording.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Whether the live view shows Voice Translate's target-language control.
static TRANSLATION_CONTROL: AtomicBool = AtomicBool::new(false);
/// Whether the live view (not a notice) is showing.
static LIVE: AtomicBool = AtomicBool::new(false);
/// The window handle of the session's captured target (0 for none). The
/// overlay is placed on that window's monitor.
static ANCHOR: AtomicI64 = AtomicI64::new(0);

/// Places the overlay on the monitor of the window a session targets.
pub(crate) fn set_anchor(window_handle: isize) {
    ANCHOR.store(window_handle as i64, Ordering::Release);
}

/// Anchors to the foreground window (or the primary monitor when there is
/// none) until a session captures its own target, so a press that fails
/// early, a take without a target, or a History Retry started from the main
/// window reports on the monitor the user is working on. The overlay is
/// never focusable, so it is never the foreground window.
pub(crate) fn anchor_to_foreground() {
    ANCHOR.store(foreground_window().unwrap_or(0) as i64, Ordering::Release);
}

#[cfg(target_os = "windows")]
fn foreground_window() -> Option<isize> {
    let window = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    (!window.is_invalid()).then_some(window.0 as isize)
}

#[cfg(not(target_os = "windows"))]
fn foreground_window() -> Option<isize> {
    None
}

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

    // Notices are click-through. The live view takes pointer events for its
    // cancel button (and Voice Translate's language control); the window is
    // never focusable, so a click does not activate it.
    window.set_ignore_cursor_events(true)?;
    place(&window, (RECORDING_WIDTH, RECORDING_HEIGHT))
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

    let live = phase_view(phase) == PhaseView::Live;
    LIVE.store(live, Ordering::Release);
    if live {
        let _ = window.set_ignore_cursor_events(click_through(phase));
        // Recalculate whenever shown in case display layout, DPI, or the
        // recording/processing window size changed.
        let _ = place(
            &window,
            live_size(TRANSLATION_CONTROL.load(Ordering::Acquire)),
        );
        let _ = window.show();
    } else {
        let _ = window.hide();
    }
}

/// Whether the overlay lets clicks through to the window below. Only views
/// with a control (the cancel button while recording or processing, and
/// Voice Translate's language control) take pointer events.
pub(crate) fn click_through(phase: &AppPhase) -> bool {
    !matches!(phase, AppPhase::Recording | AppPhase::Processing)
}

/// How a hotkey press refused while a session runs is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BusyPresentation {
    /// Flashed inside the live view, which keeps its size and controls.
    Inline,
    /// A warning notice, when no live view is showing.
    Notice,
}

pub(crate) fn busy_presentation(live_view_showing: bool) -> BusyPresentation {
    if live_view_showing {
        BusyPresentation::Inline
    } else {
        BusyPresentation::Notice
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Hint<'a> {
    message: &'a str,
    duration_ms: u64,
}

/// Reports a refused hotkey press (another mode is recording) without
/// replacing the recording's live view: a notice would hide it once its
/// timer ends while the recording continues.
pub(crate) fn show_busy(app: &AppHandle, message: &str) {
    match busy_presentation(LIVE.load(Ordering::Acquire)) {
        BusyPresentation::Inline => {
            let _ = app.emit_to(
                WINDOW_LABEL,
                "overlay-hint",
                Hint {
                    message,
                    duration_ms: HINT_DURATION.as_millis() as u64,
                },
            );
        }
        BusyPresentation::Notice => show_message_notice(app, NoticeSeverity::Warning, message),
    }
}

/// The live view's logical size.
fn live_size(translation_control: bool) -> (f64, f64) {
    let width = if translation_control {
        TRANSLATION_WIDTH
    } else {
        RECORDING_WIDTH
    };
    (width, RECORDING_HEIGHT)
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
    LIVE.store(false, Ordering::Release);
    emit(app);
    let _ = window.set_ignore_cursor_events(true);
    let _ = place(&window, (width, height));
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

/// Shows or hides Voice Translate's target-language control in the live
/// view. A notice or hidden overlay keeps its size until the next live phase.
pub(crate) fn set_interactive(app: &AppHandle, translation_control: bool) {
    TRANSLATION_CONTROL.store(translation_control, Ordering::Release);
    if !LIVE.load(Ordering::Acquire) {
        return;
    }
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = place(&window, live_size(translation_control));
    }
}

/// Sizes and moves the overlay to the bottom center of the anchor's monitor.
/// The size is converted with that monitor's scale and applied after the
/// move, so a first show on a monitor with a different DPI does not keep the
/// previous monitor's scale (and drift off center).
fn place(window: &WebviewWindow, logical_size: (f64, f64)) -> tauri::Result<()> {
    let target = match ANCHOR.load(Ordering::Acquire) {
        0 => None,
        handle => monitor_of_window(handle as isize),
    };
    let monitor = match target {
        Some(target) => {
            let mut monitors = window.available_monitors()?;
            let bounds: Vec<Rect> = monitors
                .iter()
                .map(|monitor| Rect {
                    x: monitor.position().x,
                    y: monitor.position().y,
                    width: monitor.size().width,
                    height: monitor.size().height,
                })
                .collect();
            select_monitor(&bounds, target).map(|index| monitors.swap_remove(index))
        }
        None => None,
    };
    let Some(monitor) =
        monitor.map_or_else(|| window.primary_monitor(), |monitor| Ok(Some(monitor)))?
    else {
        return Ok(());
    };
    let work_area = monitor.work_area();
    let (position, size) = placement(
        Rect {
            x: work_area.position.x,
            y: work_area.position.y,
            width: work_area.size.width,
            height: work_area.size.height,
        },
        monitor.scale_factor(),
        logical_size,
    );
    window.set_position(PhysicalPosition::new(position.0, position.1))?;
    window.set_size(PhysicalSize::new(size.0, size.1))
}

/// The physical position and size of a `logical_size` overlay centered at
/// the bottom of `work_area`, `BOTTOM_MARGIN` above its edge.
fn placement(
    work_area: Rect,
    scale_factor: f64,
    (width, height): (f64, f64),
) -> ((i32, i32), (u32, u32)) {
    let width = (width * scale_factor).round() as u32;
    let height = (height * scale_factor).round() as u32;
    let bottom_margin = (BOTTOM_MARGIN * scale_factor).round() as i32;
    let x = work_area.x + (work_area.width.saturating_sub(width) / 2) as i32;
    let y = work_area.y + work_area.height as i32 - height as i32 - bottom_margin;
    ((x, y), (width, height))
}

/// A physical-pixel rectangle in virtual-screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl Rect {
    fn overlap_area(&self, other: &Rect) -> i64 {
        let overlap = |start: i32, length: u32, other_start: i32, other_length: u32| {
            let end = (i64::from(start) + i64::from(length))
                .min(i64::from(other_start) + i64::from(other_length));
            (end - i64::from(start.max(other_start))).max(0)
        };
        overlap(self.x, self.width, other.x, other.width)
            * overlap(self.y, self.height, other.y, other.height)
    }
}

impl Rect {
    /// Squared distance between the closest points of two rectangles.
    fn gap(&self, other: &Rect) -> i64 {
        let axis = |start: i32, length: u32, other_start: i32, other_length: u32| {
            let end = i64::from(start) + i64::from(length);
            let other_end = i64::from(other_start) + i64::from(other_length);
            (i64::from(other_start) - end)
                .max(i64::from(start) - other_end)
                .max(0)
        };
        let dx = axis(self.x, self.width, other.x, other.width);
        let dy = axis(self.y, self.height, other.y, other.height);
        dx * dx + dy * dy
    }
}

/// The monitor that shows most of `target`, or the nearest one when it shows
/// none of it, like `MonitorFromWindow(MONITOR_DEFAULTTONEAREST)`. `None`
/// only when there is no monitor.
fn select_monitor(monitors: &[Rect], target: Rect) -> Option<usize> {
    let overlapping = monitors
        .iter()
        .enumerate()
        .map(|(index, monitor)| (index, monitor.overlap_area(&target)))
        .filter(|(_, area)| *area > 0)
        .max_by_key(|(index, area)| (*area, std::cmp::Reverse(*index)))
        .map(|(index, _)| index);
    overlapping.or_else(|| {
        monitors
            .iter()
            .enumerate()
            .min_by_key(|(index, monitor)| (monitor.gap(&target), *index))
            .map(|(index, _)| index)
    })
}

/// The bounds of the monitor Windows associates with the window: the one
/// showing most of it, or the nearest one. A minimized window uses its
/// restored position, so it is not sent to the primary monitor.
#[cfg(target_os = "windows")]
fn monitor_of_window(handle: isize) -> Option<Rect> {
    use windows::Win32::{
        Foundation::HWND,
        Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        },
    };
    let monitor = unsafe {
        MonitorFromWindow(
            HWND(handle as *mut core::ffi::c_void),
            MONITOR_DEFAULTTONEAREST,
        )
    };
    if monitor.is_invalid() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return None;
    }
    let bounds = info.rcMonitor;
    Some(Rect {
        x: bounds.left,
        y: bounds.top,
        width: u32::try_from(bounds.right - bounds.left).ok()?,
        height: u32::try_from(bounds.bottom - bounds.top).ok()?,
    })
}

#[cfg(not(target_os = "windows"))]
fn monitor_of_window(_: isize) -> Option<Rect> {
    None
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
    fn the_live_view_makes_room_for_the_cancel_button_and_language_control() {
        let (recording, height) = live_size(false);
        let (translating, _) = live_size(true);
        assert_eq!(height, RECORDING_HEIGHT);
        assert!(recording >= 200.0);
        assert!(translating > recording);
    }

    #[test]
    fn the_overlay_follows_the_monitor_that_shows_most_of_the_target() {
        let primary = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let left = Rect {
            x: -2560,
            y: -360,
            width: 2560,
            height: 1440,
        };
        let monitors = [primary, left];
        let window = |x, y, width, height| Rect {
            x,
            y,
            width,
            height,
        };
        assert_eq!(
            select_monitor(&monitors, window(100, 100, 800, 600)),
            Some(0)
        );
        assert_eq!(
            select_monitor(&monitors, window(-2000, 0, 800, 600)),
            Some(1)
        );
        // A maximized window overhangs its monitor's edges by a few pixels.
        assert_eq!(
            select_monitor(&monitors, window(-2568, -368, 2576, 1456)),
            Some(1)
        );
        // Straddling: the larger share wins.
        assert_eq!(
            select_monitor(&monitors, window(-300, 100, 800, 600)),
            Some(0)
        );
        assert_eq!(
            select_monitor(&monitors, window(-500, 100, 800, 600)),
            Some(1)
        );
        // Off every monitor: the nearest one, not the primary.
        assert_eq!(
            select_monitor(&monitors, window(-3000, 2000, 100, 100)),
            Some(1)
        );
        assert_eq!(
            select_monitor(&monitors, window(2500, 500, 100, 100)),
            Some(0)
        );
        // Windows reports the monitor's own bounds, which select themselves.
        assert_eq!(select_monitor(&monitors, left), Some(1));
        assert_eq!(select_monitor(&[], window(0, 0, 10, 10)), None);
    }

    #[test]
    fn placement_uses_the_target_monitors_scale() {
        let work_area = Rect {
            x: -2560,
            y: -360,
            width: 2560,
            height: 1400,
        };
        // 200x48 logical at 150%: 300x72 physical, 36 px margin.
        assert_eq!(
            placement(work_area, 1.5, (200.0, 48.0)),
            ((-2560 + 1130, -360 + 1400 - 72 - 36), (300, 72))
        );
        assert_eq!(
            placement(work_area, 1.0, (200.0, 48.0)),
            ((-2560 + 1180, -360 + 1400 - 48 - 24), (200, 48))
        );
    }

    #[test]
    fn only_views_with_a_control_take_clicks() {
        assert!(!click_through(&AppPhase::Recording));
        assert!(!click_through(&AppPhase::Processing));
        // Injecting has no cancel button: clicks reach the target.
        assert!(click_through(&AppPhase::Injecting));
        assert!(click_through(&AppPhase::Error));
    }

    #[test]
    fn a_refused_press_flashes_inside_a_showing_live_view() {
        assert_eq!(busy_presentation(true), BusyPresentation::Inline);
        assert_eq!(busy_presentation(false), BusyPresentation::Notice);
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
