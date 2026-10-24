//! Main-window access to transient application choices, never window titles/paths.
use crate::{app_context::RecentApp, Services};
use tauri::{AppHandle, Emitter, State};

#[tauri::command]
pub(crate) fn list_recent_apps(services: State<'_, Services>) -> Result<Vec<RecentApp>, String> {
    services
        .recent_apps
        .lock()
        .map(|recent| recent.list())
        .map_err(|_| "Recent apps are unavailable.".into())
}

#[tauri::command]
pub(crate) fn clear_recent_apps(
    app: AppHandle,
    services: State<'_, Services>,
) -> Result<(), String> {
    services
        .recent_apps
        .lock()
        .map_err(|_| "Recent apps are unavailable.".to_owned())?
        .clear();
    let _ = app.emit_to("main", "recent-apps-changed", ());
    Ok(())
}
