//! Content-free diagnostics over retained usage records.
use super::{Storage, StorageError};
use serde::Serialize;

#[derive(Clone, Copy)]
pub struct MetricContext<'a> {
    pub mode: crate::state::PipelineMode,
    pub app_category: Option<&'a str>,
}

impl MetricContext<'_> {
    pub(super) fn mode_code(self) -> &'static str {
        match self.mode {
            crate::state::PipelineMode::Dictate => "dictate",
            crate::state::PipelineMode::Translate => "translate",
            crate::state::PipelineMode::Edit => "edit",
            crate::state::PipelineMode::Ask => "ask",
            crate::state::PipelineMode::SelectedText => "selected_text",
        }
    }
    pub(super) fn category_code(self) -> Option<&'static str> {
        self.app_category.map(|category| {
            crate::personalization::CATEGORIES
                .into_iter()
                .find(|known| *known == category)
                .unwrap_or("other")
        })
    }
}

#[derive(Clone, Copy)]
pub struct MetricRecorder<'a> {
    storage: &'a Storage,
    context: MetricContext<'a>,
}

impl MetricRecorder<'_> {
    pub fn add_metric(
        self,
        event_type: &str,
        provider: Option<&str>,
        duration_ms: Option<i64>,
        success: bool,
        error_code: Option<&str>,
    ) -> Result<(), StorageError> {
        self.storage.add_metric_in_context(
            event_type,
            provider,
            duration_ms,
            success,
            error_code,
            Some(self.context),
        )
    }
}

/// Shape of a `take_spans` row. Diagnostics aggregate only this version, so a
/// later change of span boundaries never mixes with these records.
pub(crate) const TAKE_SPAN_VERSION: i64 = 1;

/// One timed section of a recording take, in milliseconds. A span that did
/// not happen on a take stays NULL, never zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TakeSpan {
    /// Start request to the microphone capturing.
    RecordReady,
    /// Speech-model readiness check at stop.
    ModelEnsure,
    AudioStop,
    LiveDrain,
    /// Final recognition request.
    Asr,
    /// Waiting for the shortcut keys to be released, when it was awaited.
    ShortcutRelease,
    /// Automatic voice-mode routing.
    Route,
    /// Ask action planning.
    AiPlan,
    /// The AI request of the mode (correction, translation, edit, answer).
    Ai,
    /// AI start to the first nonempty streamed output; inside `Ai`.
    AiFirstOutput,
    /// Waiting for quiet input before the final paste, when it waited.
    Settle,
    /// The output commit: paste, copy or answer-panel publish with checks.
    Paste,
    /// Dropping the live draft, which stops the input-monitor helper.
    HelperExit,
    /// The History save: before Edit/Ask complete; inside the deferred
    /// completion job for Dictate/Translate.
    HistorySave,
    /// The Dictate/Translate deferred completion job (History, dictionary
    /// candidate, metrics and temporary-audio cleanup).
    CompletionJob,
    /// Stop request to the output commit returning.
    StopToDelivery,
    /// Stop request to the lifecycle accepting a new take.
    StopToNextTake,
}

impl TakeSpan {
    pub(crate) const ALL: [Self; 17] = [
        Self::RecordReady,
        Self::ModelEnsure,
        Self::AudioStop,
        Self::LiveDrain,
        Self::Asr,
        Self::ShortcutRelease,
        Self::Route,
        Self::AiPlan,
        Self::Ai,
        Self::AiFirstOutput,
        Self::Settle,
        Self::Paste,
        Self::HelperExit,
        Self::HistorySave,
        Self::CompletionJob,
        Self::StopToDelivery,
        Self::StopToNextTake,
    ];

    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::RecordReady => "record_ready",
            Self::ModelEnsure => "model_ensure",
            Self::AudioStop => "audio_stop",
            Self::LiveDrain => "live_drain",
            Self::Asr => "asr",
            Self::ShortcutRelease => "shortcut_release",
            Self::Route => "route",
            Self::AiPlan => "plan",
            Self::Ai => "ai",
            Self::AiFirstOutput => "first_output",
            Self::Settle => "settle",
            Self::Paste => "paste",
            Self::HelperExit => "helper_exit",
            Self::HistorySave => "history_save",
            Self::CompletionJob => "completion_job",
            Self::StopToDelivery => "stop_to_delivery",
            Self::StopToNextTake => "stop_to_next_take",
        }
    }

    pub(crate) const fn index(self) -> usize {
        self as usize
    }
}

/// Where a take that produced no output ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TakeStage {
    /// Before the microphone was capturing.
    Start,
    /// While recording, before a stop.
    Recording,
    /// Audio stop, live drain and settings after a stop.
    Capture,
    /// Speech-model readiness at stop.
    Model,
    Asr,
    /// Automatic voice-mode routing.
    Route,
    /// Mode processing before output, including its AI request.
    Ai,
    /// The output commit.
    Delivery,
}

impl TakeStage {
    pub(crate) const ALL: [Self; 8] = [
        Self::Start,
        Self::Recording,
        Self::Capture,
        Self::Model,
        Self::Asr,
        Self::Route,
        Self::Ai,
        Self::Delivery,
    ];

    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Recording => "recording",
            Self::Capture => "capture",
            Self::Model => "model",
            Self::Asr => "asr",
            Self::Route => "route",
            Self::Ai => "ai",
            Self::Delivery => "delivery",
        }
    }
}

/// The single terminal outcome of a take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TakeOutcome {
    /// A confirmed paste or replacement.
    Pasted,
    PasteUnverified,
    ClipboardOnly,
    /// Neither the target nor the clipboard received the output.
    InsertionFailed,
    /// An Ask answer or notice shown in the answer panel.
    Panel,
    /// An Ask fixed search opened in the browser.
    Search,
    Failed(TakeStage),
    Cancelled(TakeStage),
}

impl TakeOutcome {
    pub(crate) const CODES: [&'static str; 8] = [
        "pasted",
        "paste_unverified",
        "clipboard_only",
        "insertion_failed",
        "panel",
        "search",
        "failed",
        "cancelled",
    ];

    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::Pasted => "pasted",
            Self::PasteUnverified => "paste_unverified",
            Self::ClipboardOnly => "clipboard_only",
            Self::InsertionFailed => "insertion_failed",
            Self::Panel => "panel",
            Self::Search => "search",
            Self::Failed(_) => "failed",
            Self::Cancelled(_) => "cancelled",
        }
    }

    pub(crate) const fn stage(self) -> Option<TakeStage> {
        match self {
            Self::Failed(stage) | Self::Cancelled(stage) => Some(stage),
            _ => None,
        }
    }
}

/// One finished take: a random take id, its recording mode, outcome and
/// spans indexed by [`TakeSpan::index`]. Content-free by construction.
#[derive(Clone, Debug)]
pub(crate) struct TakeSpanRecord {
    pub take_id: String,
    pub mode: crate::state::PipelineMode,
    pub outcome: TakeOutcome,
    pub spans: [Option<i64>; TakeSpan::ALL.len()],
}

/// Spans of one take cohort. `missing_count` counts takes of the cohort
/// without this span (or with an invalid one), so it is never read as zero.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TakeSpanSummary {
    pub span: String,
    pub timed_count: i64,
    pub missing_count: i64,
    pub p50_ms: Option<i64>,
    pub p95_ms: Option<i64>,
}

/// Takes of one recording mode with the same terminal outcome (and, for a
/// failure or cancel, the same stage). Only fixed codes; no take ids.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TakeCohortSummary {
    pub mode: Option<String>,
    pub outcome: String,
    pub stage: Option<String>,
    pub count: i64,
    pub spans: Vec<TakeSpanSummary>,
}

// A worker model identifier can contain a local path or arbitrary server
// text. Only fixed application codes may cross the diagnostics IPC boundary.
const RETAINED_METRICS: &str = "WITH retained AS (
  SELECT event_type, duration_ms, success, error_code,
    CASE WHEN mode IN ('dictate', 'translate', 'edit', 'ask', 'selected_text') THEN mode ELSE NULL END AS mode,
    CASE WHEN app_category IS NULL THEN NULL
      WHEN app_category IN ('browser', 'email', 'messaging', 'development', 'document', 'terminal', 'other') THEN app_category ELSE 'other' END AS app_category,
    CASE WHEN provider IS NULL THEN NULL
      WHEN provider LIKE 'faster-whisper:%' THEN 'faster-whisper'
      WHEN provider LIKE 'openai-compatible:%' THEN 'openai-compatible'
      WHEN provider IN ('openai', 'gemini', 'local', 'mock', 'faster-whisper',
        'vibevoice', 'openai-compatible', 'dictation', 'voice_translate',
        'speak_to_edit', 'ask', 'selected_text_translation') THEN provider
      ELSE 'other' END AS provider
  FROM metrics
)";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricSummary {
    pub event_type: String,
    pub provider: Option<String>,
    pub mode: Option<String>,
    pub app_category: Option<String>,
    pub count: i64,
    pub successes: i64,
    pub timed_count: i64,
    pub p50_ms: Option<i64>,
    pub p95_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricOutcome {
    pub event_type: String,
    pub provider: Option<String>,
    pub mode: Option<String>,
    pub app_category: Option<String>,
    pub success: bool,
    pub code: Option<String>,
    pub count: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageDiagnostics {
    pub database_ok: bool,
    pub schema_version: i64,
    pub summaries: Vec<MetricSummary>,
    pub outcomes: Vec<MetricOutcome>,
    /// Per-take spans grouped by outcome cohort (`take_spans`, version
    /// [`TAKE_SPAN_VERSION`]); kept apart from the metric summaries above.
    pub takes: Vec<TakeCohortSummary>,
}

impl Storage {
    pub fn metrics_for<'a>(
        &'a self,
        mode: crate::state::PipelineMode,
        app_category: Option<&'a str>,
    ) -> MetricRecorder<'a> {
        MetricRecorder {
            storage: self,
            context: MetricContext { mode, app_category },
        }
    }

    /// The durations of `event_type` records in the order they were written.
    #[cfg(test)]
    pub(crate) fn metric_durations_for_test(&self, event_type: &str) -> Vec<i64> {
        let connection = self.connection().unwrap();
        let mut statement = connection
            .prepare("SELECT duration_ms FROM metrics WHERE event_type = ?1 ORDER BY id")
            .unwrap();
        let durations = statement
            .query_map([event_type], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        durations
    }

    pub fn diagnostics(&self) -> Result<StorageDiagnostics, StorageError> {
        let connection = self.connection()?;
        let database_ok = connection.query_row("PRAGMA quick_check(1)", [], |row| {
            Ok(row.get::<_, String>(0)? == "ok")
        })?;
        let schema_version =
            connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        // Rank only valid timings, including failed operations. Null/negative
        // timings remain in outcome counts, never as zero-latency samples.
        // Nearest rank: ceil(N * percentile). SQLite performs the sorting;
        // retained raw events are never copied into the webview.
        let mut query = connection.prepare(&format!(
            "{RETAINED_METRICS}, ranked AS (
               SELECT event_type, provider, mode, app_category, duration_ms,
                 ROW_NUMBER() OVER (PARTITION BY event_type, provider, mode, app_category ORDER BY duration_ms) AS rank,
                 COUNT(*) OVER (PARTITION BY event_type, provider, mode, app_category) AS n
               FROM retained WHERE duration_ms >= 0
             ), timings AS (
               SELECT event_type, provider, mode, app_category, COUNT(*) AS timed_count,
                 MAX(CASE WHEN rank = (n + 1) / 2 THEN duration_ms END) AS p50,
                 MAX(CASE WHEN rank = (n * 95 + 99) / 100 THEN duration_ms END) AS p95
               FROM ranked GROUP BY event_type, provider, mode, app_category
             )
             SELECT m.event_type, m.provider, COUNT(*), SUM(m.success != 0),
               COALESCE(t.timed_count, 0), t.p50, t.p95, m.mode, m.app_category
             FROM retained m LEFT JOIN timings t
               ON m.event_type = t.event_type AND m.provider IS t.provider
                 AND m.mode IS t.mode AND m.app_category IS t.app_category
             GROUP BY m.event_type, m.provider, m.mode, m.app_category
             ORDER BY m.event_type, m.provider, m.mode, m.app_category",
        ))?;
        let summaries = query
            .query_map([], |row| {
                Ok(MetricSummary {
                    event_type: row.get(0)?,
                    provider: row.get(1)?,
                    mode: row.get(7)?,
                    app_category: row.get(8)?,
                    count: row.get(2)?,
                    successes: row.get(3)?,
                    timed_count: row.get(4)?,
                    p50_ms: row.get(5)?,
                    p95_ms: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut query = connection.prepare(&format!(
            "{RETAINED_METRICS}
             SELECT event_type, provider, success != 0, error_code, COUNT(*), mode, app_category FROM retained
             GROUP BY event_type, provider, mode, app_category, success != 0, error_code
             ORDER BY event_type, provider, mode, app_category, success, error_code",
        ))?;
        let outcomes = query
            .query_map([], |row| {
                Ok(MetricOutcome {
                    event_type: row.get(0)?,
                    provider: row.get(1)?,
                    mode: row.get(5)?,
                    app_category: row.get(6)?,
                    success: row.get(2)?,
                    code: row.get(3)?,
                    count: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let takes = take_cohorts(&connection)?;
        Ok(StorageDiagnostics {
            database_ok,
            schema_version,
            summaries,
            outcomes,
            takes,
        })
    }

    /// Writes one finished take. Like metrics, nothing is written while
    /// History is off, and retention and Delete all remove these rows
    /// together with the metrics.
    pub(crate) fn add_take_spans(&self, record: &TakeSpanRecord) -> Result<(), StorageError> {
        let connection = self.connection()?;
        if super::stored_settings(&connection)?.history_retention
            == crate::types::HistoryRetention::Never
        {
            return Ok(());
        }
        let columns: String = TakeSpan::ALL
            .iter()
            .map(|span| format!(", {}_ms", span.code()))
            .collect();
        let placeholders: String = (0..TakeSpan::ALL.len())
            .map(|index| format!(", ?{}", index + 7))
            .collect();
        let mode = MetricContext {
            mode: record.mode,
            app_category: None,
        }
        .mode_code();
        let mut values: Vec<rusqlite::types::Value> = vec![
            record.take_id.clone().into(),
            TAKE_SPAN_VERSION.into(),
            chrono::Utc::now().to_rfc3339().into(),
            mode.to_owned().into(),
            record.outcome.code().to_owned().into(),
            record
                .outcome
                .stage()
                .map(|stage| stage.code().to_owned())
                .into(),
        ];
        values.extend(record.spans.iter().map(|value| (*value).into()));
        connection.execute(
            &format!(
                "INSERT INTO take_spans(take_id, version, created_at, mode, outcome, stage{columns})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6{placeholders})"
            ),
            rusqlite::params_from_iter(values),
        )?;
        Ok(())
    }

    /// Stored takes with their present spans, in write order.
    #[cfg(test)]
    pub(crate) fn take_records_for_test(&self) -> Vec<TakeRowForTest> {
        let connection = self.connection().unwrap();
        let columns: String = TakeSpan::ALL
            .iter()
            .map(|span| format!(", {}_ms", span.code()))
            .collect();
        let mut statement = connection
            .prepare(&format!(
                "SELECT take_id, outcome, stage{columns} FROM take_spans ORDER BY id"
            ))
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                let mut spans = Vec::new();
                for (index, span) in TakeSpan::ALL.iter().enumerate() {
                    if let Some(value) = row.get::<_, Option<i64>>(index + 3)? {
                        spans.push((span.code(), value));
                    }
                }
                Ok(TakeRowForTest {
                    take_id: row.get(0)?,
                    outcome: row.get(1)?,
                    stage: row.get(2)?,
                    spans,
                })
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct TakeRowForTest {
    pub take_id: String,
    pub outcome: String,
    pub stage: Option<String>,
    pub spans: Vec<(&'static str, i64)>,
}

#[cfg(test)]
impl TakeRowForTest {
    pub(crate) fn span(&self, span: TakeSpan) -> Option<i64> {
        self.spans
            .iter()
            .find(|(code, _)| *code == span.code())
            .map(|(_, value)| *value)
    }
}

/// Aggregates current-version take records per (mode, outcome, stage) with
/// nearest-rank percentiles per span, ranked in SQLite. Stored text outside
/// the fixed codes maps to `other` (or no mode); take ids never leave SQLite.
fn take_cohorts(connection: &rusqlite::Connection) -> Result<Vec<TakeCohortSummary>, StorageError> {
    let quoted = |codes: Vec<&'static str>| {
        codes
            .into_iter()
            .map(|code| format!("'{code}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let outcomes = quoted(TakeOutcome::CODES.to_vec());
    let stages = quoted(TakeStage::ALL.iter().map(|stage| stage.code()).collect());
    let span_columns: String = TakeSpan::ALL
        .iter()
        .map(|span| format!(", {}_ms", span.code()))
        .collect();
    let unpivot = TakeSpan::ALL
        .iter()
        .map(|span| {
            let code = span.code();
            format!("SELECT mode, outcome, stage, '{code}' AS span, {code}_ms AS ms FROM takes")
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let span_list = TakeSpan::ALL
        .iter()
        .enumerate()
        .map(|(order, span)| format!("SELECT '{}' AS span, {order} AS ord", span.code()))
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let mut query = connection.prepare(&format!(
        "WITH takes AS (
           SELECT
             CASE WHEN mode IN ('dictate', 'translate', 'edit', 'ask', 'selected_text') THEN mode ELSE NULL END AS mode,
             CASE WHEN outcome IN ({outcomes}) THEN outcome ELSE 'other' END AS outcome,
             CASE WHEN stage IS NULL THEN NULL WHEN stage IN ({stages}) THEN stage ELSE 'other' END AS stage
             {span_columns}
           FROM take_spans WHERE version = {TAKE_SPAN_VERSION}
         ), cohorts AS (
           SELECT mode, outcome, stage, COUNT(*) AS n FROM takes GROUP BY mode, outcome, stage
         ), spans AS ({unpivot}
         ), ranked AS (
           SELECT mode, outcome, stage, span, ms,
             ROW_NUMBER() OVER (PARTITION BY mode, outcome, stage, span ORDER BY ms) AS rank,
             COUNT(*) OVER (PARTITION BY mode, outcome, stage, span) AS n
           FROM spans WHERE ms >= 0
         ), timings AS (
           SELECT mode, outcome, stage, span, COUNT(*) AS timed,
             MAX(CASE WHEN rank = (n + 1) / 2 THEN ms END) AS p50,
             MAX(CASE WHEN rank = (n * 95 + 99) / 100 THEN ms END) AS p95
           FROM ranked GROUP BY mode, outcome, stage, span
         ), span_list AS ({span_list})
         SELECT c.mode, c.outcome, c.stage, c.n, s.span, COALESCE(t.timed, 0), t.p50, t.p95
         FROM cohorts c CROSS JOIN span_list s
         LEFT JOIN timings t ON t.mode IS c.mode AND t.outcome = c.outcome
           AND t.stage IS c.stage AND t.span = s.span
         ORDER BY c.mode, c.outcome, c.stage, s.ord",
    ))?;
    let rows = query.query_map([], |row| {
        let count: i64 = row.get(3)?;
        let timed_count: i64 = row.get(5)?;
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            count,
            TakeSpanSummary {
                span: row.get(4)?,
                timed_count,
                missing_count: count - timed_count,
                p50_ms: row.get(6)?,
                p95_ms: row.get(7)?,
            },
        ))
    })?;
    let mut cohorts: Vec<TakeCohortSummary> = Vec::new();
    for row in rows {
        let (mode, outcome, stage, count, span) = row?;
        match cohorts.last_mut() {
            Some(last) if last.mode == mode && last.outcome == outcome && last.stage == stage => {
                last.spans.push(span);
            }
            _ => cohorts.push(TakeCohortSummary {
                mode,
                outcome,
                stage,
                count,
                spans: vec![span],
            }),
        }
    }
    Ok(cohorts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{HistoryRetention, Settings};

    #[test]
    fn mode_and_category_groups_keep_independent_percentiles_and_outcomes() {
        use crate::state::PipelineMode;
        let storage = Storage::in_memory().unwrap();
        for (mode, category, durations) in [
            (PipelineMode::Dictate, "email", vec![10, 20, 30]),
            (PipelineMode::Dictate, "messaging", vec![100, 200]),
            (PipelineMode::Translate, "email", vec![1000]),
        ] {
            for duration in durations {
                storage
                    .metrics_for(mode, Some(category))
                    .add_metric("asr", Some("faster-whisper"), Some(duration), true, None)
                    .unwrap();
            }
        }
        storage
            .metrics_for(PipelineMode::Dictate, Some("email"))
            .add_metric(
                "asr",
                Some("faster-whisper"),
                None,
                false,
                Some("asr_failed"),
            )
            .unwrap();
        storage
            .add_metric("asr", Some("faster-whisper"), Some(9999), true, None)
            .unwrap();
        let report = storage.diagnostics().unwrap();
        assert_eq!(report.summaries.len(), 4);
        let email = report
            .summaries
            .iter()
            .find(|row| {
                row.mode.as_deref() == Some("dictate")
                    && row.app_category.as_deref() == Some("email")
            })
            .unwrap();
        assert_eq!(
            (
                email.count,
                email.successes,
                email.timed_count,
                email.p50_ms,
                email.p95_ms
            ),
            (4, 3, 3, Some(20), Some(30))
        );
        let translated = report
            .summaries
            .iter()
            .find(|row| row.mode.as_deref() == Some("translate"))
            .unwrap();
        assert_eq!((translated.count, translated.p50_ms), (1, Some(1000)));
        assert!(report
            .outcomes
            .iter()
            .any(|row| row.mode.as_deref() == Some("dictate")
                && row.app_category.as_deref() == Some("email")
                && !row.success
                && row.count == 1));
        let legacy = report
            .summaries
            .iter()
            .find(|row| row.mode.is_none())
            .unwrap();
        assert!(legacy.app_category.is_none());
        assert_eq!(legacy.p50_ms, Some(9999));
    }

    #[test]
    fn metric_context_never_exports_arbitrary_category_or_mode_text() {
        let storage = Storage::in_memory().unwrap();
        storage
            .metrics_for(
                crate::state::PipelineMode::Ask,
                Some("private-category-sentinel"),
            )
            .add_metric("ask", None, Some(1), true, None)
            .unwrap();
        storage.connection().unwrap().execute("UPDATE metrics SET mode = 'private-mode-sentinel', app_category = 'private-category-sentinel'", []).unwrap();
        let report = storage.diagnostics().unwrap();
        assert!(report.summaries[0].mode.is_none());
        assert_eq!(report.summaries[0].app_category.as_deref(), Some("other"));
        assert!(!serde_json::to_string(&report).unwrap().contains("sentinel"));
    }

    #[test]
    fn reports_real_schema_and_nearest_rank_percentiles_without_missing_timings() {
        let storage = Storage::in_memory().unwrap();
        for duration in (1..=20).rev() {
            storage
                .add_metric("asr", Some("faster-whisper"), Some(duration), true, None)
                .unwrap();
        }
        storage
            .add_metric("asr", Some("faster-whisper"), None, false, Some("failed"))
            .unwrap();
        storage
            .add_metric(
                "asr",
                Some("faster-whisper"),
                Some(-1),
                false,
                Some("failed"),
            )
            .unwrap();
        storage
            .add_metric("asr", None, Some(300), true, None)
            .unwrap();
        storage
            .add_metric("insertion", None, None, false, Some("clipboard_only"))
            .unwrap();
        let result = storage.diagnostics().unwrap();
        assert!(result.database_ok);
        assert_eq!(result.schema_version, super::super::MIGRATIONS.len() as i64);
        let asr = result
            .summaries
            .iter()
            .find(|s| s.provider.as_deref() == Some("faster-whisper"))
            .unwrap();
        assert_eq!((asr.count, asr.successes, asr.timed_count), (22, 20, 20));
        assert_eq!((asr.p50_ms, asr.p95_ms), (Some(10), Some(19)));
        let insertion = result
            .summaries
            .iter()
            .find(|s| s.event_type == "insertion")
            .unwrap();
        assert_eq!((insertion.p50_ms, insertion.p95_ms), (None, None));
        assert_eq!(
            result
                .outcomes
                .iter()
                .find(|o| o.code.as_deref() == Some("failed"))
                .unwrap()
                .count,
            2
        );
        let anonymous = result
            .summaries
            .iter()
            .find(|s| s.event_type == "asr" && s.provider.is_none())
            .unwrap();
        assert_eq!((anonymous.p50_ms, anonymous.p95_ms), (Some(300), Some(300)));
    }

    #[test]
    fn arbitrary_model_identifiers_are_grouped_without_leaking_paths() {
        let storage = Storage::in_memory().unwrap();
        for (provider, duration) in [
            ("C:/private/models/sentinel", 10),
            ("private-model-sentinel", 20),
        ] {
            storage
                .add_metric("asr", Some(provider), Some(duration), true, None)
                .unwrap();
        }
        let result = storage.diagnostics().unwrap();
        assert_eq!(result.summaries.len(), 1);
        let summary = &result.summaries[0];
        assert_eq!(summary.provider.as_deref(), Some("other"));
        assert_eq!(
            (summary.count, summary.p50_ms, summary.p95_ms),
            (2, Some(10), Some(20))
        );
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("sentinel"));
        assert_eq!(result.outcomes[0].provider.as_deref(), Some("other"));
        assert_eq!(result.outcomes[0].count, 2);
    }

    #[test]
    fn legacy_worker_model_labels_keep_backend_groups_without_model_paths() {
        let storage = Storage::in_memory().unwrap();
        for provider in [
            "faster-whisper:C:/private/sentinel",
            "faster-whisper:small",
            "openai-compatible:private-sentinel",
        ] {
            storage
                .add_metric("dictation", Some(provider), Some(10), true, None)
                .unwrap();
        }
        let report = storage.diagnostics().unwrap();
        assert_eq!(report.summaries.len(), 2);
        let whisper = report
            .summaries
            .iter()
            .find(|s| s.provider.as_deref() == Some("faster-whisper"))
            .unwrap();
        assert_eq!(whisper.count, 2);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("sentinel"));
        assert!(!json.contains("small"));
    }

    fn take(outcome: TakeOutcome, spans: &[(TakeSpan, i64)]) -> TakeSpanRecord {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut record = TakeSpanRecord {
            take_id: format!("take-id-sentinel-{id}"),
            mode: crate::state::PipelineMode::Dictate,
            outcome,
            spans: [None; TakeSpan::ALL.len()],
        };
        for (span, ms) in spans {
            record.spans[span.index()] = Some(*ms);
        }
        record
    }

    #[test]
    fn every_take_span_has_a_column_in_the_schema() {
        let storage = Storage::in_memory().unwrap();
        let connection = storage.connection().unwrap();
        for span in TakeSpan::ALL {
            let exists: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('take_spans') WHERE name = ?1)",
                    [format!("{}_ms", span.code())],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "{}", span.code());
        }
        for (index, span) in TakeSpan::ALL.into_iter().enumerate() {
            assert_eq!(span.index(), index);
        }
    }

    #[test]
    fn take_cohorts_split_by_outcome_and_count_missing_spans_apart_from_zero() {
        let storage = Storage::in_memory().unwrap();
        for asr in [30, 10, 20] {
            storage
                .add_take_spans(&take(
                    TakeOutcome::Pasted,
                    &[(TakeSpan::Asr, asr), (TakeSpan::Settle, 0)],
                ))
                .unwrap();
        }
        storage
            .add_take_spans(&take(TakeOutcome::Pasted, &[(TakeSpan::Asr, 40)]))
            .unwrap();
        storage
            .add_take_spans(&take(
                TakeOutcome::Cancelled(TakeStage::Asr),
                &[(TakeSpan::AudioStop, 5)],
            ))
            .unwrap();
        // Historical metrics stay in their own summaries.
        storage
            .add_metric("asr", None, Some(9999), true, None)
            .unwrap();
        let report = storage.diagnostics().unwrap();
        assert_eq!(report.summaries.len(), 1);
        assert_eq!(report.summaries[0].p50_ms, Some(9999));
        assert_eq!(report.takes.len(), 2);
        let cancelled = &report.takes[0];
        assert_eq!(
            (cancelled.outcome.as_str(), cancelled.stage.as_deref()),
            ("cancelled", Some("asr"))
        );
        assert_eq!(cancelled.mode.as_deref(), Some("dictate"));
        let pasted = &report.takes[1];
        assert_eq!((pasted.outcome.as_str(), pasted.count), ("pasted", 4));
        assert_eq!(pasted.spans.len(), TakeSpan::ALL.len());
        let span = |code: &str| pasted.spans.iter().find(|row| row.span == code).unwrap();
        let asr = span("asr");
        assert_eq!(
            (asr.timed_count, asr.missing_count, asr.p50_ms, asr.p95_ms),
            (4, 0, Some(20), Some(40))
        );
        let settle = span("settle");
        assert_eq!(
            (settle.timed_count, settle.missing_count, settle.p50_ms),
            (3, 1, Some(0))
        );
        let paste = span("paste");
        assert_eq!(
            (paste.timed_count, paste.missing_count, paste.p50_ms),
            (0, 4, None)
        );
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("sentinel"), "take ids never leave storage");
    }

    #[test]
    fn take_cohorts_export_only_fixed_codes_and_the_current_version() {
        let storage = Storage::in_memory().unwrap();
        storage
            .add_take_spans(&take(TakeOutcome::Search, &[]))
            .unwrap();
        storage
            .add_take_spans(&take(
                TakeOutcome::Failed(TakeStage::Route),
                &[(TakeSpan::Route, 1)],
            ))
            .unwrap();
        {
            let connection = storage.connection().unwrap();
            connection
                .execute(
                    "UPDATE take_spans SET mode = 'private-mode-sentinel', outcome = 'private-outcome-sentinel', stage = 'private-stage-sentinel' WHERE outcome = 'failed'",
                    [],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO take_spans(take_id, version, created_at, outcome) VALUES ('future', 2, '2026-01-01T00:00:00+00:00', 'pasted')",
                    [],
                )
                .unwrap();
        }
        let report = storage.diagnostics().unwrap();
        let cohorts: Vec<_> = report
            .takes
            .iter()
            .map(|row| {
                (
                    row.mode.as_deref(),
                    row.outcome.as_str(),
                    row.stage.as_deref(),
                    row.count,
                )
            })
            .collect();
        assert_eq!(
            cohorts,
            [
                (None, "other", Some("other"), 1),
                (Some("dictate"), "search", None, 1)
            ]
        );
        assert!(!serde_json::to_string(&report).unwrap().contains("sentinel"));
    }

    #[test]
    fn take_records_follow_delete_all_retention_and_disabled_history() {
        let storage = Storage::in_memory().unwrap();
        storage
            .add_take_spans(&take(TakeOutcome::Pasted, &[(TakeSpan::Asr, 1)]))
            .unwrap();
        storage.delete_all_history().unwrap();
        assert!(storage.take_records_for_test().is_empty());
        assert!(storage.diagnostics().unwrap().takes.is_empty());

        storage
            .add_take_spans(&take(TakeOutcome::Pasted, &[(TakeSpan::Asr, 1)]))
            .unwrap();
        storage
            .connection()
            .unwrap()
            .execute(
                "UPDATE take_spans SET created_at = '2000-01-01T00:00:00+00:00'",
                [],
            )
            .unwrap();
        storage
            .add_take_spans(&take(TakeOutcome::Pasted, &[(TakeSpan::Asr, 2)]))
            .unwrap();
        storage
            .update_settings_and_apply_history_policy(&Settings {
                history_retention: HistoryRetention::OneWeek,
                ..Settings::default()
            })
            .unwrap();
        let rows = storage.take_records_for_test();
        assert_eq!(rows.len(), 1, "an expired take is purged by retention");
        assert_eq!(rows[0].span(TakeSpan::Asr), Some(2));

        storage
            .update_settings_and_apply_history_policy(&Settings {
                history_retention: HistoryRetention::Never,
                ..Settings::default()
            })
            .unwrap();
        assert!(storage.take_records_for_test().is_empty());
        storage
            .add_take_spans(&take(TakeOutcome::Pasted, &[]))
            .unwrap();
        assert!(storage.take_records_for_test().is_empty());
    }

    #[test]
    fn diagnostics_respects_history_deletion_and_disabled_retention() {
        let storage = Storage::in_memory().unwrap();
        storage
            .add_metric("asr", None, Some(10), true, None)
            .unwrap();
        storage.delete_all_history().unwrap();
        assert!(storage.diagnostics().unwrap().summaries.is_empty());
        storage
            .update_settings_and_apply_history_policy(&Settings {
                history_retention: HistoryRetention::Never,
                ..Settings::default()
            })
            .unwrap();
        storage
            .add_metric("asr", None, Some(10), true, None)
            .unwrap();
        let result = storage.diagnostics().unwrap();
        assert!(result.summaries.is_empty());
        assert!(result.outcomes.is_empty());
    }
}
