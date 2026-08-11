use tauri::{
    AppHandle, Manager, PhysicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};

const WINDOW_LABEL: &str = "recording-overlay";
const WINDOW_WIDTH: f64 = 172.0;
const WINDOW_HEIGHT: f64 = 48.0;
const BOTTOM_MARGIN: f64 = 24.0;

pub(crate) fn create(app: &AppHandle) -> tauri::Result<()> {
    let window = WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("index.html".into()))
        .title("Recording")
        .inner_size(WINDOW_WIDTH, WINDOW_HEIGHT)
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

    // The indicator is informational and must never intercept clicks intended
    // for the app underneath it.
    window.set_ignore_cursor_events(true)?;
    position_on_primary_monitor(&window)
}

pub(crate) fn set_recording(app: &AppHandle, recording: bool) {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };

    if recording {
        // Recalculate on every recording in case display layout or DPI changed.
        let _ = position_on_primary_monitor(&window);
        let _ = window.show();
    } else {
        let _ = window.hide();
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
