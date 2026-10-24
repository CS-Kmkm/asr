//! Text-only History actions are restricted to the main window.
use crate::{
    pipeline::history_text::{self, Action},
    types::RecordingResult,
};
use tauri::AppHandle;

#[tauri::command]
pub(crate) async fn reprocess_history_text(
    app: AppHandle,
    id: i64,
    action: Action,
) -> Result<RecordingResult, String> {
    tauri::async_runtime::spawn(history_text::run(app, id, action))
        .await
        .map_err(|_| "History retry failed.".to_owned())?
}
