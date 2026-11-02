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
pub(crate) mod speculative;
pub(crate) mod stop;
pub(crate) mod take_trace;

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
    /// Opt-in speculative correction of the last live hypothesis, started
    /// after stop; `None` when it does not apply.
    speculation: Option<speculative::Speculation>,
    stop_timings: StopTimings,
    /// Spans and outcome of this take; disabled for dictation tests.
    trace: take_trace::TakeTrace,
}

/// Stop-path stage timings. They are written after delivery (or after a
/// recognition failure) so their storage commits stay off the measured path.
#[derive(Clone, Copy)]
struct StopTimings {
    audio_stop_ms: i64,
    live_drain_ms: Option<i64>,
    /// Whether the last live hypothesis of a Dictate take equals its final
    /// transcript exactly; `None` without a hypothesis or a final transcript.
    live_final_match: Option<bool>,
}

/// Content-free record of whether a Dictate take's last live hypothesis
/// equals its final transcript (`success`), whatever the speculative
/// correction setting. Mismatches carry the fixed code `differs`.
const LIVE_FINAL_MATCH: &str = "live_final_match";

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
        if let Some(matched) = self.live_final_match {
            let _ = metrics.add_metric(
                LIVE_FINAL_MATCH,
                Some(asr_backend),
                None,
                matched,
                (!matched).then_some("differs"),
            );
        }
    }
}

/// Whether the last live hypothesis of a stopped take equals its final
/// transcript. Only Dictate takes that showed live text are compared.
fn live_final_match(
    mode: crate::state::PipelineMode,
    hypothesis: Option<&str>,
    transcript: &str,
) -> Option<bool> {
    (mode == crate::state::PipelineMode::Dictate)
        .then_some(hypothesis)
        .flatten()
        .map(|hypothesis| hypothesis == transcript)
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
            live_final_match: None,
        }
        .record(metrics, "faster-whisper");
        assert_eq!(timed_events(&storage), vec![("audio_stop".into(), 1)]);

        StopTimings {
            audio_stop_ms: 15,
            live_drain_ms: Some(0),
            live_final_match: None,
        }
        .record(metrics, "faster-whisper");
        assert_eq!(
            timed_events(&storage),
            vec![("audio_stop".into(), 2), ("live_drain".into(), 1)]
        );
    }

    #[test]
    fn live_final_match_compares_only_dictate_takes_that_showed_live_text() {
        use super::live_final_match;
        assert_eq!(
            live_final_match(PipelineMode::Dictate, Some("hello"), "hello"),
            Some(true)
        );
        // Exact comparison: whitespace or punctuation differences are misses.
        assert_eq!(
            live_final_match(PipelineMode::Dictate, Some("hello"), "hello."),
            Some(false)
        );
        assert_eq!(live_final_match(PipelineMode::Dictate, None, "hello"), None);
        assert_eq!(
            live_final_match(PipelineMode::Translate, Some("hello"), "hello"),
            None
        );
    }

    #[test]
    fn live_final_match_is_recorded_with_the_stop_timings_without_text() {
        let storage = Storage::in_memory().unwrap();
        let metrics = storage.metrics_for(PipelineMode::Dictate, None);
        for live_final_match in [Some(true), Some(false), None] {
            StopTimings {
                audio_stop_ms: 1,
                live_drain_ms: Some(1),
                live_final_match,
            }
            .record(metrics, "faster-whisper");
        }
        let report = storage.diagnostics().unwrap();
        let summary = report
            .summaries
            .iter()
            .find(|row| row.event_type == "live_final_match")
            .expect("match records");
        assert_eq!((summary.count, summary.successes), (2, 1));
        assert_eq!(summary.timed_count, 0);
        let codes: Vec<_> = report
            .outcomes
            .iter()
            .filter(|row| row.event_type == "live_final_match")
            .map(|row| (row.success, row.code.clone(), row.count))
            .collect();
        assert_eq!(
            codes,
            vec![(false, Some("differs".to_owned()), 1), (true, None, 1)]
        );
    }
}
