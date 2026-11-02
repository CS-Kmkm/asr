//! Opt-in speculative AI correction for Dictate (audit item C3).
//!
//! After stop, while the recording is still recognized, the last live
//! hypothesis is corrected with exactly the request the normal correction
//! would build for that text: the same function with the settings snapshot,
//! the dictionary hints for that text and app context, and the style
//! guidance. Its streamed deltas are never shown. When the normal correction
//! would start, the result is adopted only if the final transcript equals
//! the hypothesis exactly and every input recomputed for the final
//! transcript is equal; otherwise the speculative request is dropped, which
//! cancels it, and the normal correction runs without waiting for it. A
//! failed speculation also falls back to the normal correction, silently.
//!
//! Content-free `speculative_correction` metric, attributed to the
//! correction provider: `success` only for a hit, whose duration is the
//! whole speculative request; otherwise the fixed codes in [`Outcome::code`].
//! A take that ends before that decision (cancellation, recognition or
//! routing failure) drops the request and records nothing.
use crate::{
    correction::{self, CorrectionError},
    dictionary::CorrectionTerm,
    personalization,
    state::PipelineMode,
    storage::{MetricRecorder, Storage},
    types::{AppContext, Settings},
};
use std::{
    future::Future,
    sync::{Arc, OnceLock},
    time::Instant,
};
use tokio::sync::watch;

pub(crate) const METRIC_EVENT: &str = "speculative_correction";

/// Receives streamed provider text. The speculative one only notes when
/// the first nonempty text arrived; nothing is displayed.
pub(crate) type OnUpdate = Box<dyn FnMut(&str) + Send>;

/// The dictionary hints the normal correction sends for `text`.
pub(super) fn correction_hints(
    storage: &Storage,
    text: &str,
    app_context: Option<&AppContext>,
) -> Vec<CorrectionTerm> {
    storage
        .dictionary_correction_terms(text, app_context)
        .unwrap_or_default()
}

/// The style guidance the normal correction sends.
pub(super) fn style_guidance(
    settings: &Settings,
    app_context: Option<&AppContext>,
) -> Option<String> {
    personalization::resolve_profile(settings, app_context)
        .map(|profile| personalization::guidance(&profile))
}

/// Everything besides the text that determines a correction request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CorrectionInputs {
    pub(crate) settings: Settings,
    pub(crate) hints: Vec<CorrectionTerm>,
    pub(crate) guidance: Option<String>,
}

impl CorrectionInputs {
    /// The inputs the normal correction computes for `text`.
    pub(super) fn resolve(
        storage: &Storage,
        settings: &Settings,
        text: &str,
        app_context: Option<&AppContext>,
    ) -> Self {
        Self {
            settings: settings.clone(),
            hints: correction_hints(storage, text, app_context),
            guidance: style_guidance(settings, app_context),
        }
    }

    fn matches(
        &self,
        settings: &Settings,
        hints: &[CorrectionTerm],
        guidance: Option<&str>,
    ) -> bool {
        self.settings == *settings && self.hints == hints && self.guidance.as_deref() == guidance
    }
}

/// The production request: exactly the normal correction call.
pub(super) async fn provider_correction(
    inputs: CorrectionInputs,
    text: String,
    cancel: watch::Receiver<bool>,
    on_update: OnUpdate,
) -> Result<String, CorrectionError> {
    correction::correct_transcript(
        &inputs.settings,
        &text,
        &inputs.hints,
        inputs.guidance.as_deref(),
        cancel,
        on_update,
    )
    .await
}

/// Why an enabled speculation sent nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotStarted {
    /// Live preview showed no text (or withdrew all of it).
    NoHypothesis,
    /// The correction provider lacks the consent the normal path requires.
    Consent,
}

/// How an enabled speculation ended, as recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Adopted; the duration is the whole speculative request.
    Hit {
        request_ms: i64,
    },
    /// The final transcript differs from the hypothesis.
    MissText,
    /// The text matched but a correction input differs.
    MissInputs,
    /// The take was delivered as another mode than Dictate.
    MissMode,
    /// The speculative request failed; the normal correction ran.
    Failed,
    NotStarted(NotStarted),
}

impl Outcome {
    /// Fixed, content-free code; `None` for a hit.
    pub(crate) fn code(self) -> Option<&'static str> {
        Some(match self {
            Self::Hit { .. } => return None,
            Self::MissText => "miss_text",
            Self::MissInputs => "miss_inputs",
            Self::MissMode => "miss_mode",
            Self::Failed => "failed",
            Self::NotStarted(NotStarted::NoHypothesis) => "not_started:no_hypothesis",
            Self::NotStarted(NotStarted::Consent) => "not_started:consent",
        })
    }

    fn record(self, metrics: MetricRecorder<'_>, provider: &str) {
        let request_ms = match self {
            Self::Hit { request_ms } => Some(request_ms),
            _ => None,
        };
        let _ = metrics.add_metric(
            METRIC_EVENT,
            Some(provider),
            request_ms,
            request_ms.is_some(),
            self.code(),
        );
    }
}

/// An adopted speculative correction.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Adopted {
    pub(crate) text: String,
    /// Time from the adoption point to the first streamed text, zero when
    /// it arrived earlier; `None` when no text was streamed. Only the
    /// per-take first-output span uses it: the `text_correction_first_output`
    /// metric is not recorded for a hit, and its `text_correction` record is
    /// untimed.
    pub(crate) first_output_ms: Option<i64>,
}

/// The speculative request's task, aborted (its request dropped) when this
/// is dropped before it finished.
struct AbortOnDrop(tokio::task::JoinHandle<(Result<String, CorrectionError>, Instant)>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) struct Running {
    hypothesis: String,
    inputs: CorrectionInputs,
    started: Instant,
    first_output: Arc<OnceLock<Instant>>,
    task: AbortOnDrop,
}

/// The speculation of one take, from stop until its decision.
pub(crate) enum Speculation {
    NotStarted(NotStarted),
    Running(Box<Running>),
}

impl Speculation {
    /// Starts a speculation for a stopped take, or returns `None` when it
    /// does not apply (setting off, AI correction off, or not Dictate), in
    /// which case nothing is sent or recorded. The caller excludes dictation
    /// tests and cancelled takes. `correct` must send exactly the normal
    /// correction request ([`provider_correction`] in production).
    pub(super) fn begin<C, F>(
        settings: &Settings,
        mode: PipelineMode,
        hypothesis: Option<&str>,
        resolve_inputs: impl FnOnce(&str) -> CorrectionInputs,
        cancel: watch::Receiver<bool>,
        correct: C,
    ) -> Option<Self>
    where
        C: FnOnce(CorrectionInputs, String, watch::Receiver<bool>, OnUpdate) -> F,
        F: Future<Output = Result<String, CorrectionError>> + Send + 'static,
    {
        if mode != PipelineMode::Dictate
            || !settings.speculative_correction
            || !settings.text_correction_enabled
        {
            return None;
        }
        let Some(hypothesis) = hypothesis.filter(|text| !text.trim().is_empty()) else {
            return Some(Self::NotStarted(NotStarted::NoHypothesis));
        };
        // The same gate the normal request applies before sending.
        if crate::privacy::check_text(settings).is_err() {
            return Some(Self::NotStarted(NotStarted::Consent));
        }
        let inputs = resolve_inputs(hypothesis);
        let first_output = Arc::new(OnceLock::new());
        let noted = Arc::clone(&first_output);
        let request = correct(
            inputs.clone(),
            hypothesis.to_owned(),
            cancel,
            Box::new(move |delta: &str| {
                if !delta.is_empty() {
                    let _ = noted.set(Instant::now());
                }
            }),
        );
        let started = Instant::now();
        let task = tokio::spawn(async move {
            let result = request.await;
            (result, Instant::now())
        });
        Some(Self::Running(Box::new(Running {
            hypothesis: hypothesis.to_owned(),
            inputs,
            started,
            first_output,
            task: AbortOnDrop(task),
        })))
    }

    /// Decides at the point where the normal correction of the delivered
    /// Dictate `transcript` would start (`adoption_point`), given the inputs
    /// it computed. Returns the result to use instead of a new request, or
    /// `None` to run the normal correction; a dropped speculative request is
    /// cancelled, never awaited. Records the outcome, except when the take
    /// was cancelled while waiting (the normal path then reports that).
    pub(super) async fn adopt(
        self,
        transcript: &str,
        settings: &Settings,
        hints: &[CorrectionTerm],
        guidance: Option<&str>,
        adoption_point: Instant,
        metrics: MetricRecorder<'_>,
    ) -> Option<Adopted> {
        let provider = settings.correction_provider.as_str();
        let running = match self {
            Self::NotStarted(reason) => {
                Outcome::NotStarted(reason).record(metrics, provider);
                return None;
            }
            Self::Running(running) => running,
        };
        if running.hypothesis != transcript {
            drop(running);
            Outcome::MissText.record(metrics, provider);
            return None;
        }
        if !running.inputs.matches(settings, hints, guidance) {
            drop(running);
            Outcome::MissInputs.record(metrics, provider);
            return None;
        }
        let Running {
            started,
            first_output,
            mut task,
            ..
        } = *running;
        match (&mut task.0).await {
            Ok((Ok(text), finished)) => {
                Outcome::Hit {
                    request_ms: finished.saturating_duration_since(started).as_millis() as i64,
                }
                .record(metrics, provider);
                Some(Adopted {
                    text,
                    first_output_ms: first_output
                        .get()
                        .map(|at| at.saturating_duration_since(adoption_point).as_millis() as i64),
                })
            }
            // Only the take's cancellation cancels the request it was given.
            Ok((Err(CorrectionError::Cancelled), _)) => None,
            Ok((Err(_), _)) | Err(_) => {
                Outcome::Failed.record(metrics, provider);
                None
            }
        }
    }

    /// Ends a speculation whose take was delivered as another mode: drops
    /// (cancels) its request and records why it was not used.
    pub(super) fn discard(self, metrics: MetricRecorder<'_>, provider: &str) {
        match self {
            Self::NotStarted(reason) => Outcome::NotStarted(reason).record(metrics, provider),
            Self::Running(running) => {
                drop(running);
                Outcome::MissMode.record(metrics, provider);
            }
        }
    }
}

#[cfg(test)]
mod tests;
