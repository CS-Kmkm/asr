use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
};

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

use crate::types::{DictionaryEntry, HistoryItem, NewDictionaryEntry, NewHistoryItem, Settings};

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("database operation failed")]
    Database(#[from] rusqlite::Error),
    #[error("stored settings are invalid")]
    InvalidSettings(#[from] serde_json::Error),
    #[error("storage lock is unavailable")]
    Lock,
}

pub struct Storage {
    connection: Mutex<Connection>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let connection = Connection::open(path)?;
        let storage = Self {
            connection: Mutex::new(connection),
        };
        storage.migrate()?;
        Ok(storage)
    }

    #[cfg(test)]
    fn in_memory() -> Result<Self, StorageError> {
        let storage = Self {
            connection: Mutex::new(Connection::open_in_memory()?),
        };
        storage.migrate()?;
        Ok(storage)
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, StorageError> {
        self.connection.lock().map_err(|_| StorageError::Lock)
    }

    fn migrate(&self) -> Result<(), StorageError> {
        self.connection()?.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS settings (
               key TEXT PRIMARY KEY NOT NULL,
               value TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS dictionary_entries (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               reading TEXT NOT NULL,
               surface TEXT NOT NULL,
               category TEXT,
               aliases TEXT NOT NULL DEFAULT '[]',
               priority INTEGER NOT NULL DEFAULT 0,
               app_scope TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS dictation_history (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               transcript_text TEXT NOT NULL,
               processed_text TEXT,
               mode TEXT NOT NULL,
               asr_provider TEXT NOT NULL,
               llm_provider TEXT,
               app_category TEXT,
               duration_ms INTEGER,
               latency_ms INTEGER,
               created_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_history_created_at ON dictation_history(created_at DESC);
             CREATE TABLE IF NOT EXISTS metrics (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               event_type TEXT NOT NULL,
               provider TEXT,
               duration_ms INTEGER,
               success INTEGER NOT NULL,
               error_code TEXT,
               created_at TEXT NOT NULL
             );",
        )?;
        Ok(())
    }

    pub fn get_settings(&self) -> Result<Settings, StorageError> {
        let stored: Option<String> = self
            .connection()?
            .query_row(
                "SELECT value FROM settings WHERE key = 'app_settings'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        stored
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map(|value| value.unwrap_or_default())
            .map_err(Into::into)
    }

    pub fn update_settings(&self, settings: &Settings) -> Result<(), StorageError> {
        let value = serde_json::to_string(settings)?;
        self.connection()?.execute(
            "INSERT INTO settings(key, value, updated_at) VALUES('app_settings', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![value, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn apply_history_policy(
        &self,
        previous: &Settings,
        settings: &Settings,
    ) -> Result<(), StorageError> {
        let connection = self.connection()?;
        if previous.history_enabled && !settings.history_enabled {
            connection.execute("DELETE FROM dictation_history", [])?;
            return Ok(());
        }
        if settings.history_enabled {
            connection.execute(
                "DELETE FROM dictation_history
                 WHERE datetime(created_at) < datetime('now', ?1)",
                [format!("-{} days", settings.history_retention_days)],
            )?;
        }
        Ok(())
    }

    pub fn enforce_current_history_policy(&self) -> Result<(), StorageError> {
        let settings = self.get_settings()?;
        self.apply_history_policy(&settings, &settings)
    }

    pub fn add_history(&self, item: &NewHistoryItem<'_>) -> Result<bool, StorageError> {
        if !self.get_settings()?.history_enabled {
            return Ok(false);
        }
        self.connection()?.execute(
            "INSERT INTO dictation_history(
               transcript_text, processed_text, mode, asr_provider, llm_provider,
               app_category, duration_ms, latency_ms, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                item.transcript_text,
                item.processed_text,
                item.mode,
                item.asr_provider,
                item.llm_provider,
                item.app_category,
                item.duration_ms,
                item.latency_ms,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(true)
    }

    pub fn list_history(&self, limit: u32) -> Result<Vec<HistoryItem>, StorageError> {
        if !self.get_settings()?.history_enabled {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, transcript_text, processed_text, mode, asr_provider, llm_provider,
                    app_category, duration_ms, latency_ms, created_at
             FROM dictation_history ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit.min(500)], |row| {
            Ok(HistoryItem {
                id: row.get(0)?,
                transcript_text: row.get(1)?,
                processed_text: row.get(2)?,
                mode: row.get(3)?,
                asr_provider: row.get(4)?,
                llm_provider: row.get(5)?,
                app_category: row.get(6)?,
                duration_ms: row.get(7)?,
                latency_ms: row.get(8)?,
                created_at: row.get(9)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn history_text(&self, id: i64) -> Result<Option<String>, StorageError> {
        self.connection()?.query_row(
            "SELECT COALESCE(processed_text, transcript_text) FROM dictation_history WHERE id = ?1",
            [id],
            |row| row.get(0),
        ).optional().map_err(Into::into)
    }

    pub fn list_dictionary(&self) -> Result<Vec<DictionaryEntry>, StorageError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, reading, surface, category, aliases, priority, app_scope, created_at
             FROM dictionary_entries ORDER BY priority DESC, surface ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?;
        rows.map(|row| {
            let (id, reading, surface, category, aliases, priority, app_scope, created_at) = row?;
            Ok(DictionaryEntry {
                id,
                reading,
                surface,
                category,
                aliases: serde_json::from_str(&aliases)?,
                priority,
                app_scope,
                created_at,
            })
        })
        .collect()
    }

    pub fn add_dictionary_entry(
        &self,
        entry: &NewDictionaryEntry<'_>,
    ) -> Result<i64, StorageError> {
        let aliases = serde_json::to_string(entry.aliases)?;
        let now = Utc::now().to_rfc3339();
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO dictionary_entries(
               reading, surface, category, aliases, priority, app_scope, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![
                entry.reading,
                entry.surface,
                entry.category,
                aliases,
                entry.priority,
                entry.app_scope,
                now
            ],
        )?;
        Ok(connection.last_insert_rowid())
    }

    pub fn delete_dictionary_entry(&self, id: i64) -> Result<bool, StorageError> {
        Ok(self
            .connection()?
            .execute("DELETE FROM dictionary_entries WHERE id = ?1", [id])?
            > 0)
    }

    pub fn dictionary_prompt_terms(&self) -> Result<Vec<String>, StorageError> {
        let mut terms = Vec::new();
        for entry in self.list_dictionary()? {
            terms.push(entry.surface);
            terms.extend(entry.aliases);
        }
        Ok(terms)
    }

    pub fn dictionary_correction_hints(
        &self,
        transcript: &str,
    ) -> Result<Vec<String>, StorageError> {
        let transcript = transcript.to_lowercase();
        Ok(self
            .list_dictionary()?
            .into_iter()
            .filter_map(|entry| {
                let surface = entry.surface.trim();
                let surface_folded = surface.to_lowercase();
                let mut matched = Vec::new();
                for variant in std::iter::once(entry.reading.as_str())
                    .chain(entry.aliases.iter().map(String::as_str))
                {
                    let variant = variant.trim();
                    if variant.is_empty() || variant.to_lowercase() == surface_folded {
                        continue;
                    }
                    if transcript.contains(&variant.to_lowercase())
                        && !matched.iter().any(|current| current == variant)
                    {
                        matched.push(variant.to_owned());
                    }
                }
                (!matched.is_empty()).then(|| format!("{surface}<={}", matched.join("|")))
            })
            .collect())
    }

    pub fn add_metric(
        &self,
        event_type: &str,
        provider: Option<&str>,
        duration_ms: Option<i64>,
        success: bool,
        error_code: Option<&str>,
    ) -> Result<(), StorageError> {
        self.connection()?.execute(
            "INSERT INTO metrics(event_type, provider, duration_ms, success, error_code, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event_type,
                provider,
                duration_ms,
                i64::from(success),
                error_code,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item<'a>() -> NewHistoryItem<'a> {
        NewHistoryItem {
            transcript_text: "private transcript",
            processed_text: Some("processed transcript"),
            mode: "faithful",
            asr_provider: "test",
            llm_provider: None,
            app_category: None,
            duration_ms: Some(1000),
            latency_ms: Some(200),
        }
    }

    #[test]
    fn creates_all_required_tables() {
        let storage = Storage::in_memory().unwrap();
        let connection = storage.connection().unwrap();
        for table in [
            "settings",
            "dictation_history",
            "metrics",
            "dictionary_entries",
        ] {
            let exists: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "missing table {table}");
        }
    }

    #[test]
    fn settings_round_trip() {
        let storage = Storage::in_memory().unwrap();
        let mut settings = Settings::default();
        settings.hotkey = "Ctrl+Alt+V".into();
        settings.history_retention_days = 7;
        settings.custom_models.push(crate::types::CustomModel {
            asr_backend: "faster-whisper".into(),
            model_id: "community/whisper-custom".into(),
        });
        storage.update_settings(&settings).unwrap();
        assert_eq!(storage.get_settings().unwrap(), settings);
    }

    #[test]
    fn legacy_settings_receive_automatic_editing_defaults() {
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        let object = value.as_object_mut().unwrap();
        for field in [
            "correctionRemoveFillers",
            "correctionRemoveRepetitions",
            "correctionResolveSelfCorrections",
            "correctionAutoFormat",
            "correctionImproveClarity",
        ] {
            object.remove(field);
        }
        let settings: Settings = serde_json::from_value(value).unwrap();
        assert!(settings.correction_remove_fillers);
        assert!(settings.correction_remove_repetitions);
        assert!(settings.correction_resolve_self_corrections);
        assert!(settings.correction_auto_format);
        assert!(settings.correction_improve_clarity);
    }

    #[test]
    fn defaults_select_faster_whisper_backend() {
        let storage = Storage::in_memory().unwrap();
        assert_eq!(
            storage.get_settings().unwrap().asr_backend,
            "faster-whisper"
        );
    }

    #[test]
    fn asr_backend_round_trips() {
        let storage = Storage::in_memory().unwrap();
        let mut settings = Settings::default();
        settings.asr_backend = "faster-whisper".into();
        storage.update_settings(&settings).unwrap();
        assert_eq!(
            storage.get_settings().unwrap().asr_backend,
            "faster-whisper"
        );
    }

    #[test]
    fn history_disabled_never_stores_transcript() {
        let storage = Storage::in_memory().unwrap();
        let mut settings = Settings::default();
        settings.history_enabled = false;
        storage.update_settings(&settings).unwrap();

        assert!(!storage.add_history(&item()).unwrap());
        let count: i64 = storage
            .connection()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM dictation_history", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn history_returns_processed_text_for_copy() {
        let storage = Storage::in_memory().unwrap();
        storage.add_history(&item()).unwrap();
        let id = storage.list_history(10).unwrap()[0].id;
        assert_eq!(
            storage.history_text(id).unwrap().as_deref(),
            Some("processed transcript")
        );
    }

    fn dictionary_entry<'a>(aliases: &'a [String]) -> NewDictionaryEntry<'a> {
        NewDictionaryEntry {
            reading: "おーぷんえーあい",
            surface: "OpenAI",
            category: Some("organization"),
            aliases,
            priority: 10,
            app_scope: Some("global"),
        }
    }

    #[test]
    fn dictionary_add_and_list_round_trip() {
        let storage = Storage::in_memory().unwrap();
        let aliases = vec!["ChatGPT".into()];
        let id = storage
            .add_dictionary_entry(&dictionary_entry(&aliases))
            .unwrap();
        let entries = storage.list_dictionary().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, id);
        assert_eq!(entries[0].surface, "OpenAI");
        assert_eq!(entries[0].priority, 10);
    }

    #[test]
    fn dictionary_delete_reports_missing_rows() {
        let storage = Storage::in_memory().unwrap();
        let aliases = Vec::new();
        let id = storage
            .add_dictionary_entry(&dictionary_entry(&aliases))
            .unwrap();
        assert!(storage.delete_dictionary_entry(id).unwrap());
        assert!(!storage.delete_dictionary_entry(id).unwrap());
    }

    #[test]
    fn dictionary_aliases_json_round_trip() {
        let storage = Storage::in_memory().unwrap();
        let aliases = vec!["ChatGPT".into(), "GPT".into()];
        storage
            .add_dictionary_entry(&dictionary_entry(&aliases))
            .unwrap();
        assert_eq!(storage.list_dictionary().unwrap()[0].aliases, aliases);
    }

    #[test]
    fn dictionary_prompt_terms_include_surfaces() {
        let storage = Storage::in_memory().unwrap();
        let aliases = vec!["ChatGPT".into()];
        storage
            .add_dictionary_entry(&dictionary_entry(&aliases))
            .unwrap();
        let terms = storage.dictionary_prompt_terms().unwrap();
        assert!(terms.contains(&"OpenAI".to_string()));
        assert!(terms.contains(&"ChatGPT".to_string()));
    }

    #[test]
    fn dictionary_correction_hints_map_aliases_to_preferred_surface() {
        let storage = Storage::in_memory().unwrap();
        let aliases = vec!["Chat GPT".into()];
        storage
            .add_dictionary_entry(&dictionary_entry(&aliases))
            .unwrap();
        assert_eq!(
            storage
                .dictionary_correction_hints("Chat GPTを使います")
                .unwrap(),
            vec!["OpenAI<=Chat GPT"]
        );
        assert!(storage
            .dictionary_correction_hints("関係のない文章")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn disabling_history_purges_existing_rows() {
        let storage = Storage::in_memory().unwrap();
        storage.add_history(&item()).unwrap();
        let previous = storage.get_settings().unwrap();
        let mut settings = previous.clone();
        settings.history_enabled = false;
        storage.apply_history_policy(&previous, &settings).unwrap();
        assert_eq!(
            storage
                .connection()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM dictation_history", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn retention_removes_only_expired_history() {
        let storage = Storage::in_memory().unwrap();
        storage.add_history(&item()).unwrap();
        storage
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO dictation_history(
                   transcript_text, mode, asr_provider, created_at
                 ) VALUES ('old', 'faithful', 'test', datetime('now', '-40 days'))",
                [],
            )
            .unwrap();
        let settings = storage.get_settings().unwrap();
        storage.apply_history_policy(&settings, &settings).unwrap();
        assert_eq!(storage.list_history(10).unwrap().len(), 1);
    }
}
