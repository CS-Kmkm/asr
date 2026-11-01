//! Decisions shared by initial processing and History reprocessing.
pub(crate) mod model_load;
use crate::correction::CorrectionError;

#[derive(Debug)]
pub(crate) enum AiStageResult {
    Corrected(String),
    /// A failed optional AI stage preserves the original transcript.
    Fallback(CorrectionError),
    /// Cancellation never converts into a successful transcript fallback.
    Cancelled,
}

pub(crate) fn classify_ai_stage(result: Result<String, CorrectionError>) -> AiStageResult {
    match result {
        Ok(text) => AiStageResult::Corrected(text),
        Err(CorrectionError::Cancelled) => AiStageResult::Cancelled,
        Err(error) => AiStageResult::Fallback(error),
    }
}

mod ask;
mod dictate_translate;
mod edit;
mod routing;
pub(crate) mod setup_tests;
pub(crate) mod stop;

#[derive(Clone, Copy)]
pub(crate) struct PipelineContext<'a> {
    app: &'a tauri::AppHandle,
    services: &'a crate::Services,
    state: &'a crate::state::AppState,
    storage: &'a crate::storage::Storage,
    operation_id: u64,
}

struct RecordedTake {
    automatic: bool,
    settings: crate::types::Settings,
    transcript: crate::asr::Transcript,
    artifact: crate::audio::AudioArtifact,
    artifact_cleanup: crate::TempArtifact,
    started: std::time::Instant,
    correction_cancel: tokio::sync::watch::Receiver<bool>,
    mode: crate::state::PipelineMode,
    app_context: Option<crate::types::AppContext>,
    translation_target: Option<String>,
    draft: Option<crate::live_dictation::LiveDraft>,
    stop_timings: StopTimings,
}

/// Stop-path stage timings. They are written after delivery (or after a
/// recognition failure) so their storage commits stay off the measured path.
#[derive(Clone, Copy)]
struct StopTimings {
    audio_stop_ms: i64,
    live_drain_ms: Option<i64>,
}

impl StopTimings {
    fn record(self, metrics: crate::storage::MetricRecorder<'_>, asr_backend: &str) {
        let _ = metrics.add_metric("audio_stop", None, Some(self.audio_stop_ms), true, None);
        if let Some(live_drain_ms) = self.live_drain_ms {
            let _ = metrics.add_metric(
                "live_drain",
                Some(asr_backend),
                Some(live_drain_ms),
                true,
                None,
            );
        }
    }
}

pub(crate) mod finish;
pub(crate) mod history_text;
pub(crate) mod personalization;
pub(crate) mod retry;

mod orchestration;

#[cfg(test)]
mod tests {
    use super::StopTimings;
    use crate::{state::PipelineMode, storage::Storage};

    fn timed_events(storage: &Storage) -> Vec<(String, i64)> {
        let mut events: Vec<_> = storage
            .diagnostics()
            .unwrap()
            .summaries
            .into_iter()
            .map(|row| (row.event_type, row.timed_count))
            .collect();
        events.sort();
        events
    }

    #[test]
    fn stop_timings_record_live_drain_only_when_a_live_task_was_joined() {
        let storage = Storage::in_memory().unwrap();
        let metrics = storage.metrics_for(PipelineMode::Edit, None);
        StopTimings {
            audio_stop_ms: 12,
            live_drain_ms: None,
        }
        .record(metrics, "faster-whisper");
        assert_eq!(timed_events(&storage), vec![("audio_stop".into(), 1)]);

        StopTimings {
            audio_stop_ms: 15,
            live_drain_ms: Some(0),
        }
        .record(metrics, "faster-whisper");
        assert_eq!(
            timed_events(&storage),
            vec![("audio_stop".into(), 2), ("live_drain".into(), 1)]
        );
    }
}
