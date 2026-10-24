//! Persistence and cleanup shared by live operations and History Retry.
use crate::{emit_status, storage::Storage, types::NewHistoryItem, TempArtifact};
use tauri::AppHandle;

pub(crate) fn save_history(
    storage: &Storage,
    item: &NewHistoryItem<'_>,
    audio_path: Option<&std::path::Path>,
) -> HistorySaveStatus {
    match storage.add_history_with_audio_report(item, audio_path) {
        Ok((_, true)) => HistorySaveStatus::AudioUnavailable,
        Ok((_, false)) => HistorySaveStatus::Complete,
        Err(_) => HistorySaveStatus::Failed,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistorySaveStatus {
    Complete,
    AudioUnavailable,
    Failed,
}

pub(crate) fn report_history_save_warning(app: &AppHandle, status: HistorySaveStatus) {
    match status {
        HistorySaveStatus::Complete => {}
        HistorySaveStatus::AudioUnavailable => emit_status(
            app,
            "history_audio_unavailable",
            "History text was saved, but the recording could not be retained.",
        ),
        HistorySaveStatus::Failed => emit_status(
            app,
            "history_save_failed",
            "The result completed, but History could not be saved.",
        ),
    }
}

pub(crate) fn report_temp_cleanup(app: &AppHandle, artifact: &mut TempArtifact) {
    if artifact.cleanup().is_err() {
        emit_status(
            app,
            "artifact_cleanup_failed",
            "Temporary audio cleanup failed.",
        );
    }
}

/// Saves a successful Retry of History row `source_id`. A failed take kept
/// only for Retry then follows the History and audio settings; if that
/// cleanup fails, the take still expires with the 24-hour purge.
pub(crate) fn save_retry_result(
    storage: &Storage,
    item: &NewHistoryItem<'_>,
    audio_path: &std::path::Path,
    source_id: i64,
) -> HistorySaveStatus {
    let status = save_history(storage, item, Some(audio_path));
    if status != HistorySaveStatus::Failed {
        let _ = storage.settle_retried_failed_take(source_id);
    }
    status
}
