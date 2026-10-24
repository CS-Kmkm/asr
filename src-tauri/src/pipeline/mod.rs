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
}

pub(crate) mod finish;
pub(crate) mod history_text;
pub(crate) mod retry;

mod orchestration;
