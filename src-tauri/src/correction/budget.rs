//! Task budgets do not depend on prompt text, wording, or locale suffixes.
use super::{max_output_tokens, ASK_PLAN_MIN_OUTPUT_TOKENS, ASK_TEXT_MIN_OUTPUT_TOKENS};

#[derive(Clone, Copy)]
pub(super) enum OutputBudget {
    Transcript,
    Edit,
    AskPlan,
    AskText,
}

impl OutputBudget {
    pub(super) fn minimum(self) -> Option<usize> {
        match self {
            Self::AskPlan => Some(ASK_PLAN_MIN_OUTPUT_TOKENS),
            Self::AskText => Some(ASK_TEXT_MIN_OUTPUT_TOKENS),
            _ => None,
        }
    }

    pub(super) fn visible_tokens(self, input: &str) -> usize {
        match self {
            Self::Edit => input
                .chars()
                .count()
                .saturating_mul(4)
                .saturating_add(1024)
                .clamp(2048, 32_768),
            _ => max_output_tokens(input).max(self.minimum().unwrap_or_default()),
        }
    }
}
