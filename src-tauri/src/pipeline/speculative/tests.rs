use super::*;
use std::{future::pending, sync::Mutex, time::Duration};
use tokio::sync::oneshot;

const WAIT: Duration = Duration::from_secs(5);

fn enabled_settings() -> Settings {
    Settings {
        text_correction_enabled: true,
        speculative_correction: true,
        // The local provider needs no cloud consent.
        correction_provider: "local".into(),
        ..Settings::default()
    }
}

fn inputs_for(settings: &Settings) -> CorrectionInputs {
    CorrectionInputs {
        settings: settings.clone(),
        hints: vec![CorrectionTerm {
            surface: "Tauri".into(),
            readings: vec!["たうり".into()],
        }],
        guidance: Some("Prefer concise phrasing.".into()),
    }
}

/// What the fake provider was asked to correct.
type Calls = Arc<Mutex<Vec<(String, CorrectionInputs)>>>;

/// Sets its flag when the request future holding it is dropped.
struct DropFlag(Option<oneshot::Sender<()>>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

enum Reply {
    /// Streams these deltas, then returns the text.
    Text(&'static [&'static str], &'static str),
    Error,
    /// Never finishes unless the take is cancelled.
    UntilCancelled,
}

/// A fake provider request, owned like the production future.
type FakeRequest = std::pin::Pin<Box<dyn Future<Output = Result<String, CorrectionError>> + Send>>;

fn fake(
    calls: Calls,
    reply: Reply,
    dropped: oneshot::Sender<()>,
) -> impl FnOnce(CorrectionInputs, String, watch::Receiver<bool>, OnUpdate) -> FakeRequest {
    move |inputs, text, mut cancel, mut on_update| {
        calls.lock().unwrap().push((text, inputs));
        // Owned by the request future, so dropping it unpolled also counts.
        let flag = DropFlag(Some(dropped));
        Box::pin(async move {
            let _flag = flag;
            match reply {
                Reply::Text(deltas, text) => {
                    for delta in deltas {
                        on_update(delta);
                    }
                    Ok(text.to_owned())
                }
                Reply::Error => Err(CorrectionError::InvalidResponse("fake failure".into())),
                Reply::UntilCancelled => {
                    while !*cancel.borrow() {
                        if cancel.changed().await.is_err() {
                            pending::<()>().await;
                        }
                    }
                    Err(CorrectionError::Cancelled)
                }
            }
        })
    }
}

struct Fixture {
    storage: Storage,
    settings: Settings,
    calls: Calls,
    dropped: oneshot::Receiver<()>,
    cancel: watch::Sender<bool>,
}

impl Fixture {
    fn new() -> Self {
        Self::with(enabled_settings())
    }

    fn with(settings: Settings) -> Self {
        Self {
            storage: Storage::in_memory().unwrap(),
            settings,
            calls: Arc::default(),
            dropped: oneshot::channel().1,
            cancel: watch::channel(false).0,
        }
    }

    fn begin(
        &mut self,
        mode: PipelineMode,
        hypothesis: Option<&str>,
        reply: Reply,
    ) -> Option<Speculation> {
        let (dropped, dropped_rx) = oneshot::channel();
        self.dropped = dropped_rx;
        let settings = self.settings.clone();
        Speculation::begin(
            &self.settings,
            mode,
            hypothesis,
            |_| inputs_for(&settings),
            self.cancel.subscribe(),
            fake(Arc::clone(&self.calls), reply, dropped),
        )
    }

    fn metrics(&self) -> MetricRecorder<'_> {
        self.storage.metrics_for(PipelineMode::Dictate, None)
    }

    async fn adopt(
        &self,
        speculation: Speculation,
        transcript: &str,
        inputs: &CorrectionInputs,
    ) -> Option<Adopted> {
        tokio::time::timeout(
            WAIT,
            speculation.adopt(
                transcript,
                &inputs.settings,
                &inputs.hints,
                inputs.guidance.as_deref(),
                Instant::now(),
                self.metrics(),
            ),
        )
        .await
        .expect("adoption never waits for a dropped request")
    }

    /// `(success, code, count, timed)` of each speculative outcome.
    fn outcomes(&self) -> Vec<(bool, Option<String>, i64)> {
        self.storage
            .diagnostics()
            .unwrap()
            .outcomes
            .into_iter()
            .filter(|row| row.event_type == METRIC_EVENT)
            .map(|row| (row.success, row.code, row.count))
            .collect()
    }

    fn timed_hits(&self) -> i64 {
        self.storage
            .diagnostics()
            .unwrap()
            .summaries
            .into_iter()
            .find(|row| row.event_type == METRIC_EVENT)
            .map_or(0, |row| row.timed_count)
    }

    fn sent(&self) -> Vec<(String, CorrectionInputs)> {
        self.calls.lock().unwrap().clone()
    }

    /// Waits until the speculative request future was dropped (cancelled
    /// or finished).
    async fn request_dropped(&mut self) {
        tokio::time::timeout(WAIT, &mut self.dropped)
            .await
            .expect("the speculative request was dropped")
            .expect("drop flag");
    }
}

fn miss(code: &str) -> Vec<(bool, Option<String>, i64)> {
    vec![(false, Some(code.to_owned()), 1)]
}

#[tokio::test]
async fn setting_off_correction_off_or_another_mode_sends_and_records_nothing() {
    let off = Settings {
        speculative_correction: false,
        ..enabled_settings()
    };
    let no_correction = Settings {
        text_correction_enabled: false,
        ..enabled_settings()
    };
    for (settings, mode) in [
        (off, PipelineMode::Dictate),
        (no_correction, PipelineMode::Dictate),
        (enabled_settings(), PipelineMode::Translate),
        (enabled_settings(), PipelineMode::Edit),
        (enabled_settings(), PipelineMode::Ask),
    ] {
        let mut fixture = Fixture::with(settings);
        assert!(fixture
            .begin(mode, Some("hello"), Reply::Text(&[], "Hello."))
            .is_none());
        assert!(fixture.sent().is_empty());
        assert!(fixture.outcomes().is_empty());
    }
}

#[tokio::test]
async fn without_a_hypothesis_or_consent_nothing_is_sent_and_the_reason_is_recorded() {
    for hypothesis in [None, Some(""), Some("  ")] {
        let mut fixture = Fixture::new();
        let speculation = fixture
            .begin(
                PipelineMode::Dictate,
                hypothesis,
                Reply::Text(&[], "Hello."),
            )
            .unwrap();
        assert!(matches!(
            speculation,
            Speculation::NotStarted(NotStarted::NoHypothesis)
        ));
        let inputs = inputs_for(&fixture.settings);
        assert_eq!(fixture.adopt(speculation, "hello", &inputs).await, None);
        assert!(fixture.sent().is_empty());
        assert_eq!(fixture.outcomes(), miss("not_started:no_hypothesis"));
    }

    let mut fixture = Fixture::with(Settings {
        correction_provider: "openai".into(),
        cloud_consents: Vec::new(),
        ..enabled_settings()
    });
    let speculation = fixture
        .begin(
            PipelineMode::Dictate,
            Some("hello"),
            Reply::Text(&[], "Hello."),
        )
        .unwrap();
    assert!(fixture.sent().is_empty());
    let inputs = inputs_for(&fixture.settings);
    assert_eq!(fixture.adopt(speculation, "hello", &inputs).await, None);
    assert_eq!(fixture.outcomes(), miss("not_started:consent"));
}

#[tokio::test]
async fn an_identical_final_transcript_and_inputs_adopt_the_result_without_showing_deltas() {
    let mut fixture = Fixture::new();
    let speculation = fixture
        .begin(
            PipelineMode::Dictate,
            Some("hello world"),
            Reply::Text(&["Hel", "Hello, world."], "Hello, world."),
        )
        .unwrap();
    let inputs = inputs_for(&fixture.settings);
    // The request carries exactly the hypothesis and its inputs.
    tokio::time::timeout(WAIT, async {
        while fixture.sent().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fixture.sent(),
        vec![("hello world".to_owned(), inputs.clone())]
    );
    fixture.request_dropped().await;
    let adopted = fixture
        .adopt(speculation, "hello world", &inputs)
        .await
        .expect("hit");
    // Only the validated final text is returned; streamed deltas were not
    // forwarded anywhere, and output that arrived before the adoption point
    // counts as an immediate first output.
    assert_eq!(
        adopted,
        Adopted {
            text: "Hello, world.".into(),
            first_output_ms: Some(0),
        }
    );
    assert_eq!(fixture.outcomes(), vec![(true, None, 1)]);
    assert_eq!(fixture.timed_hits(), 1);
    assert_eq!(fixture.sent().len(), 1);
}

#[tokio::test]
async fn a_hit_without_streamed_text_has_no_first_output() {
    let mut fixture = Fixture::new();
    let speculation = fixture
        .begin(
            PipelineMode::Dictate,
            Some("hello"),
            Reply::Text(&["", ""], "Hello."),
        )
        .unwrap();
    let inputs = inputs_for(&fixture.settings);
    let adopted = fixture.adopt(speculation, "hello", &inputs).await.unwrap();
    assert_eq!(adopted.first_output_ms, None);
}

#[tokio::test]
async fn a_different_final_transcript_cancels_the_request_without_waiting() {
    let mut fixture = Fixture::new();
    let speculation = fixture
        .begin(PipelineMode::Dictate, Some("hello"), Reply::UntilCancelled)
        .unwrap();
    let inputs = inputs_for(&fixture.settings);
    // Exact comparison: trailing punctuation is already a miss.
    assert_eq!(fixture.adopt(speculation, "hello.", &inputs).await, None);
    fixture.request_dropped().await;
    assert_eq!(fixture.outcomes(), miss("miss_text"));
}

#[tokio::test]
async fn any_changed_input_is_a_miss_that_cancels_the_request() {
    let base = inputs_for(&enabled_settings());
    let changed = [
        CorrectionInputs {
            settings: Settings {
                correction_mode: "intent_aware".into(),
                ..base.settings.clone()
            },
            ..base.clone()
        },
        CorrectionInputs {
            settings: Settings {
                local_correction_model: "another-model".into(),
                ..base.settings.clone()
            },
            ..base.clone()
        },
        CorrectionInputs {
            hints: Vec::new(),
            ..base.clone()
        },
        CorrectionInputs {
            guidance: None,
            ..base.clone()
        },
    ];
    for inputs in changed {
        let mut fixture = Fixture::new();
        let speculation = fixture
            .begin(PipelineMode::Dictate, Some("hello"), Reply::UntilCancelled)
            .unwrap();
        assert_eq!(fixture.adopt(speculation, "hello", &inputs).await, None);
        fixture.request_dropped().await;
        assert_eq!(fixture.outcomes(), miss("miss_inputs"));
    }
}

#[tokio::test]
async fn a_failed_speculation_falls_back_silently() {
    let mut fixture = Fixture::new();
    let speculation = fixture
        .begin(PipelineMode::Dictate, Some("hello"), Reply::Error)
        .unwrap();
    let inputs = inputs_for(&fixture.settings);
    assert_eq!(fixture.adopt(speculation, "hello", &inputs).await, None);
    assert_eq!(fixture.outcomes(), miss("failed"));
}

#[tokio::test]
async fn cancelling_the_take_cancels_the_request_and_records_nothing() {
    let mut fixture = Fixture::new();
    let speculation = fixture
        .begin(PipelineMode::Dictate, Some("hello"), Reply::UntilCancelled)
        .unwrap();
    fixture.cancel.send_replace(true);
    fixture.request_dropped().await;
    let inputs = inputs_for(&fixture.settings);
    assert_eq!(fixture.adopt(speculation, "hello", &inputs).await, None);
    assert!(fixture.outcomes().is_empty());
}

#[tokio::test]
async fn a_take_that_ends_before_the_decision_drops_the_request() {
    let mut fixture = Fixture::new();
    let speculation = fixture
        .begin(PipelineMode::Dictate, Some("hello"), Reply::UntilCancelled)
        .unwrap();
    drop(speculation);
    fixture.request_dropped().await;
    assert!(fixture.outcomes().is_empty());
}

#[tokio::test]
async fn routing_away_from_dictate_discards_the_request_as_a_mode_miss() {
    let mut fixture = Fixture::new();
    let speculation = fixture
        .begin(PipelineMode::Dictate, Some("hello"), Reply::UntilCancelled)
        .unwrap();
    speculation.discard(fixture.metrics(), "local");
    fixture.request_dropped().await;
    assert_eq!(fixture.outcomes(), miss("miss_mode"));

    let mut fixture = Fixture::new();
    fixture
        .begin(PipelineMode::Dictate, None, Reply::UntilCancelled)
        .unwrap()
        .discard(fixture.metrics(), "local");
    assert_eq!(fixture.outcomes(), miss("not_started:no_hypothesis"));
}

#[test]
fn inputs_resolve_like_the_normal_path_and_see_dictionary_changes() {
    let storage = Storage::in_memory().unwrap();
    let settings = enabled_settings();
    let text = "たうりのアプリ";
    let before = CorrectionInputs::resolve(&storage, &settings, text, None);
    assert_eq!(
        before,
        CorrectionInputs::resolve(&storage, &settings, text, None)
    );
    assert_eq!(before.hints, correction_hints(&storage, text, None));
    assert_eq!(before.guidance, style_guidance(&settings, None));
    storage
        .add_dictionary_entry(&crate::types::NewDictionaryEntry {
            reading: "たうり",
            surface: "Tauri",
            category: None,
            aliases: &[],
            priority: 0,
            app_scope: None,
        })
        .unwrap();
    let after = CorrectionInputs::resolve(&storage, &settings, text, None);
    assert_ne!(before, after);
    assert!(!after.matches(&settings, &before.hints, before.guidance.as_deref()));
    assert!(after.matches(&settings, &after.hints, after.guidance.as_deref()));
}

#[test]
fn outcome_codes_are_fixed_and_content_free() {
    let codes: Vec<_> = [
        Outcome::Hit { request_ms: 1 },
        Outcome::MissText,
        Outcome::MissInputs,
        Outcome::MissMode,
        Outcome::Failed,
        Outcome::NotStarted(NotStarted::NoHypothesis),
        Outcome::NotStarted(NotStarted::Consent),
    ]
    .into_iter()
    .map(Outcome::code)
    .collect();
    assert_eq!(
        codes,
        vec![
            None,
            Some("miss_text"),
            Some("miss_inputs"),
            Some("miss_mode"),
            Some("failed"),
            Some("not_started:no_hypothesis"),
            Some("not_started:consent"),
        ]
    );
}
