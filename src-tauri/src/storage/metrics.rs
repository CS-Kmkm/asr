//! Content-free diagnostics over retained usage records.
use super::{Storage, StorageError};
use serde::Serialize;

// A worker model identifier can contain a local path or arbitrary server
// text. Only fixed application codes may cross the diagnostics IPC boundary.
const RETAINED_METRICS: &str = "WITH retained AS (
  SELECT event_type, duration_ms, success, error_code,
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
}

impl Storage {
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
               SELECT event_type, provider, duration_ms,
                 ROW_NUMBER() OVER (PARTITION BY event_type, provider ORDER BY duration_ms) AS rank,
                 COUNT(*) OVER (PARTITION BY event_type, provider) AS n
               FROM retained WHERE duration_ms >= 0
             ), timings AS (
               SELECT event_type, provider, COUNT(*) AS timed_count,
                 MAX(CASE WHEN rank = (n + 1) / 2 THEN duration_ms END) AS p50,
                 MAX(CASE WHEN rank = (n * 95 + 99) / 100 THEN duration_ms END) AS p95
               FROM ranked GROUP BY event_type, provider
             )
             SELECT m.event_type, m.provider, COUNT(*), SUM(m.success != 0),
               COALESCE(t.timed_count, 0), t.p50, t.p95
             FROM retained m LEFT JOIN timings t
               ON m.event_type = t.event_type AND m.provider IS t.provider
             GROUP BY m.event_type, m.provider
             ORDER BY m.event_type, m.provider",
        ))?;
        let summaries = query
            .query_map([], |row| {
                Ok(MetricSummary {
                    event_type: row.get(0)?,
                    provider: row.get(1)?,
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
             SELECT event_type, provider, success != 0, error_code, COUNT(*) FROM retained
             GROUP BY event_type, provider, success != 0, error_code
             ORDER BY event_type, provider, success, error_code",
        ))?;
        let outcomes = query
            .query_map([], |row| {
                Ok(MetricOutcome {
                    event_type: row.get(0)?,
                    provider: row.get(1)?,
                    success: row.get(2)?,
                    code: row.get(3)?,
                    count: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(StorageDiagnostics {
            database_ok,
            schema_version,
            summaries,
            outcomes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{HistoryRetention, Settings};

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
