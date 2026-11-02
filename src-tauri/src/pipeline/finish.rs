//! Persistence and cleanup shared by live operations and History Retry.
use crate::{emit_status, storage::Storage, types::NewHistoryItem, TempArtifact};
use tauri::AppHandle;

/// Main-window events, without payload, after a deferred completion job
/// wrote a History row or a dictionary candidate; the History and
/// Dictionary pages re-read their data on them.
pub(crate) const HISTORY_CHANGED_EVENT: &str = "history-changed";
pub(crate) const DICTIONARY_CHANGED_EVENT: &str = "dictionary-changed";

pub(crate) fn save_history(
    storage: &Storage,
    item: &NewHistoryItem<'_>,
    audio_path: Option<&std::path::Path>,
) -> HistorySaveStatus {
    save_history_reporting_row(storage, item, audio_path).0
}

/// Like [`save_history`], and also tells whether a row was written (History
/// off writes none).
pub(crate) fn save_history_reporting_row(
    storage: &Storage,
    item: &NewHistoryItem<'_>,
    audio_path: Option<&std::path::Path>,
) -> (HistorySaveStatus, bool) {
    match storage.add_history_with_audio_report(item, audio_path) {
        Ok((saved, true)) => (HistorySaveStatus::AudioUnavailable, saved),
        Ok((saved, false)) => (HistorySaveStatus::Complete, saved),
        Err(_) => (HistorySaveStatus::Failed, false),
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

/// The status kind and message of a failed temporary-audio cleanup.
pub(crate) const TEMP_CLEANUP_FAILED: (&str, &str) =
    ("artifact_cleanup_failed", "Temporary audio cleanup failed.");

pub(crate) fn report_temp_cleanup(app: &AppHandle, artifact: &mut TempArtifact) {
    if artifact.cleanup().is_err() {
        let (kind, message) = TEMP_CLEANUP_FAILED;
        emit_status(app, kind, message);
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
