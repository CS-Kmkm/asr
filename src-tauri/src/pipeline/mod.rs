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
