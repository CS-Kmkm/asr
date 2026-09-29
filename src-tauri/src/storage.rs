use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
};

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

use crate::types::{
    DictionaryCandidate, DictionaryEntry, HistoryItem, NewDictionaryEntry, NewHistoryItem, Settings,
};

const MAX_DICTIONARY_TEXT_CHARS: usize = 200;
const MAX_CANDIDATE_CHARS: usize = 80;
const MAX_CSV_BYTES: usize = 1024 * 1024;
const MAX_CSV_ROWS: usize = 1000;

fn scope_matches(scope: Option<&str>, context: Option<&crate::types::AppContext>) -> bool {
    let Some(scope) = scope.filter(|value| !value.is_empty() && *value != "global") else {
        return true;
    };
    let Some(context) = context else {
        return false;
    };
    let app_scope = context.app_key.as_deref().map(|key| format!("app:{key}"));
    app_scope.as_deref() == Some(scope) || scope == format!("category:{}", context.category)
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("database operation failed")]
    Database(#[from] rusqlite::Error),
    #[error("stored settings are invalid")]
    InvalidSettings(#[from] serde_json::Error),
    #[error("storage lock is unavailable")]
    Lock,
    #[error("dictionary validation failed: {0}")]
    Validation(String),
    #[error("CSV parsing failed")]
    Csv(#[from] csv::Error),
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
               source TEXT NOT NULL DEFAULT 'manual',
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS dictionary_candidates (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               original_span TEXT NOT NULL,
               preferred_span TEXT NOT NULL,
               confidence REAL NOT NULL,
               history_id INTEGER,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               FOREIGN KEY(history_id) REFERENCES dictation_history(id) ON DELETE SET NULL
             );
             CREATE INDEX IF NOT EXISTS idx_dictionary_candidates_created_at ON dictionary_candidates(created_at DESC);
             CREATE TABLE IF NOT EXISTS dictionary_candidate_decisions (
               original_span TEXT NOT NULL,
               preferred_span TEXT NOT NULL,
               created_at TEXT NOT NULL,
               PRIMARY KEY(original_span, preferred_span)
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
        let connection = self.connection()?;
        let columns = {
            let mut statement = connection.prepare("PRAGMA table_info(dictionary_entries)")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let has_source = columns.iter().any(|column| column == "source");
        if !has_source {
            connection.execute(
                "ALTER TABLE dictionary_entries ADD COLUMN source TEXT NOT NULL DEFAULT 'manual'",
                [],
            )?;
        }
        connection.execute("UPDATE dictionary_entries SET source = 'manual' WHERE source IS NULL OR source NOT IN ('manual', 'auto')", [])?;
        let decision_columns = {
            let mut statement =
                connection.prepare("PRAGMA table_info(dictionary_candidate_decisions)")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if !decision_columns.iter().any(|column| column == "created_at") {
            // An unreleased development database may have the old table. Its
            // undated private spans cannot be retained under a time policy.
            connection.execute(
                "ALTER TABLE dictionary_candidate_decisions ADD COLUMN created_at TEXT",
                [],
            )?;
        }
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
        _previous: &Settings,
        settings: &Settings,
    ) -> Result<(), StorageError> {
        let connection = self.connection()?;
        Self::apply_history_policy_on_connection(&connection, settings)
    }

    fn apply_history_policy_on_connection(
        connection: &Connection,
        settings: &Settings,
    ) -> Result<(), StorageError> {
        if !settings.history_enabled {
            connection.execute("DELETE FROM dictation_history", [])?;
            connection.execute("DELETE FROM dictionary_candidates", [])?;
            connection.execute("DELETE FROM dictionary_candidate_decisions", [])?;
            return Ok(());
        }
        if settings.history_enabled {
            let cutoff = format!("-{} days", settings.history_retention_days);
            connection.execute(
                "DELETE FROM dictation_history
                 WHERE datetime(created_at) < datetime('now', ?1)",
                [&cutoff],
            )?;
            connection.execute(
                "DELETE FROM dictionary_candidates WHERE datetime(created_at) < datetime('now', ?1)",
                [&cutoff],
            )?;
            connection.execute(
                "DELETE FROM dictionary_candidate_decisions WHERE created_at IS NULL OR datetime(created_at) < datetime('now', ?1)",
                [&cutoff],
            )?;
        }
        Ok(())
    }

    pub fn update_settings_with_history_policy(
        &self,
        settings: &Settings,
    ) -> Result<(), StorageError> {
        let value = serde_json::to_string(settings)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        Self::apply_history_policy_on_connection(&transaction, settings)?;
        transaction.execute(
            "INSERT INTO settings(key, value, updated_at) VALUES('app_settings', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![value, Utc::now().to_rfc3339()],
        )?;
        transaction.commit()?;
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
        read_dictionary_entries(&self.connection()?)
    }

    pub fn search_dictionary(
        &self,
        query: Option<&str>,
        source: Option<&str>,
    ) -> Result<Vec<DictionaryEntry>, StorageError> {
        let source = source.filter(|value| !value.is_empty() && *value != "all");
        if !matches!(source, None | Some("manual") | Some("auto")) {
            return Err(StorageError::Validation(
                "source must be manual or auto".into(),
            ));
        }
        let query = query.unwrap_or("").trim().to_lowercase();
        Ok(self
            .list_dictionary()?
            .into_iter()
            .filter(|entry| source.is_none_or(|value| entry.source == value))
            .filter(|entry| {
                query.is_empty()
                    || [
                        entry.reading.as_str(),
                        entry.surface.as_str(),
                        entry.category.as_deref().unwrap_or(""),
                        &entry.aliases.join(" "),
                        entry.app_scope.as_deref().unwrap_or(""),
                    ]
                    .iter()
                    .any(|value| value.to_lowercase().contains(&query))
            })
            .collect())
    }

    pub fn add_dictionary_entry(
        &self,
        entry: &NewDictionaryEntry<'_>,
    ) -> Result<i64, StorageError> {
        let connection = self.connection()?;
        insert_dictionary_entry(&connection, entry, "manual", None)
    }

    pub fn update_dictionary_entry(
        &self,
        id: i64,
        entry: &NewDictionaryEntry<'_>,
    ) -> Result<bool, StorageError> {
        let connection = self.connection()?;
        let existing = read_dictionary_entries(&connection)?;
        if !existing.iter().any(|current| current.id == id) {
            return Ok(false);
        }
        validate_dictionary_entry(entry)?;
        validate_dictionary_collisions(&existing, entry, Some(id))?;
        let scope = normalized_scope(entry.app_scope);
        Ok(connection.execute(
            "UPDATE dictionary_entries SET reading = ?1, surface = ?2, category = ?3, aliases = ?4,
             priority = ?5, app_scope = ?6, updated_at = ?7 WHERE id = ?8",
            params![
                entry.reading.trim(),
                entry.surface.trim(),
                entry
                    .category
                    .map(str::trim)
                    .filter(|value| !value.is_empty()),
                serde_json::to_string(entry.aliases)?,
                entry.priority,
                scope,
                Utc::now().to_rfc3339(),
                id
            ],
        )? > 0)
    }

    pub fn import_dictionary_csv(&self, csv_text: &str) -> Result<usize, StorageError> {
        if csv_text.contains('\u{fffd}') {
            return Err(StorageError::Validation(
                "CSV contains replacement characters; check its encoding".into(),
            ));
        }
        if csv_text.len() > MAX_CSV_BYTES {
            return Err(StorageError::Validation("CSV input exceeds 1 MiB".into()));
        }
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_reader(csv_text.as_bytes());
        let headers = reader.headers()?;
        let expected = [
            "reading",
            "surface",
            "category",
            "aliases",
            "priority",
            "app_scope",
        ];
        if headers.iter().collect::<Vec<_>>() != expected {
            return Err(StorageError::Validation(
                "CSV header must be reading,surface,category,aliases,priority,app_scope".into(),
            ));
        }
        let mut values = Vec::new();
        for (index, row) in reader.records().enumerate() {
            if index >= MAX_CSV_ROWS {
                return Err(StorageError::Validation(
                    "CSV has more than 1000 rows".into(),
                ));
            }
            let row = row?;
            if row.len() != expected.len() {
                return Err(StorageError::Validation(format!(
                    "CSV row {} has an invalid field count",
                    index + 2
                )));
            }
            let priority = row[4].trim().parse::<i64>().map_err(|_| {
                StorageError::Validation(format!("CSV row {} has an invalid priority", index + 2))
            })?;
            values.push(OwnedDictionaryEntry {
                reading: row[0].to_owned(),
                surface: row[1].to_owned(),
                category: (!row[2].trim().is_empty()).then(|| row[2].to_owned()),
                aliases: if row[3].trim().is_empty() {
                    Vec::new()
                } else {
                    row[3]
                        .split('|')
                        .map(str::trim)
                        .map(ToOwned::to_owned)
                        .collect()
                },
                priority,
                app_scope: (!row[5].trim().is_empty()).then(|| row[5].to_owned()),
            });
        }
        let connection = self.connection()?;
        let mut planned = read_dictionary_entries(&connection)?;
        for value in &values {
            let entry = value.as_new();
            validate_dictionary_entry(&entry)?;
            validate_dictionary_collisions(&planned, &entry, None)?;
            planned.push(DictionaryEntry {
                id: -((planned.len() as i64) + 1),
                reading: entry.reading.trim().into(),
                surface: entry.surface.trim().into(),
                category: entry
                    .category
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
                aliases: entry.aliases.to_vec(),
                priority: entry.priority,
                app_scope: normalized_scope(entry.app_scope),
                source: "manual".into(),
                created_at: String::new(),
            });
        }
        let transaction = connection.unchecked_transaction()?;
        for value in &values {
            insert_dictionary_entry(&transaction, &value.as_new(), "manual", None)?;
        }
        transaction.commit()?;
        Ok(values.len())
    }

    pub fn delete_dictionary_entry(&self, id: i64) -> Result<bool, StorageError> {
        Ok(self
            .connection()?
            .execute("DELETE FROM dictionary_entries WHERE id = ?1", [id])?
            > 0)
    }

    pub fn add_dictionary_candidate_from_correction(
        &self,
        original: &str,
        corrected: &str,
        history_id: Option<i64>,
    ) -> Result<Option<i64>, StorageError> {
        let Some((original_span, preferred_span)) =
            detect_dictionary_candidate(original, corrected)
        else {
            return Ok(None);
        };
        let now = Utc::now().to_rfc3339();
        let connection = self.connection()?;
        let stored_settings: Option<String> = connection
            .query_row(
                "SELECT value FROM settings WHERE key = 'app_settings'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let history_enabled = stored_settings
            .map(|value| serde_json::from_str::<Settings>(&value))
            .transpose()?
            .unwrap_or_default()
            .history_enabled;
        if !history_enabled {
            return Ok(None);
        }
        let already_seen: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM dictionary_candidates WHERE original_span = ?1 AND preferred_span = ?2
              UNION SELECT 1 FROM dictionary_candidate_decisions WHERE original_span = ?1 AND preferred_span = ?2)",
            params![original_span, preferred_span], |row| row.get(0),
        )?;
        if already_seen
            || read_dictionary_entries(&connection)?.iter().any(|entry| {
                normalized_scope(entry.app_scope.as_deref()).is_none()
                    && entry.surface.eq_ignore_ascii_case(&preferred_span)
            })
        {
            return Ok(None);
        }
        connection.execute(
            "INSERT INTO dictionary_candidates(original_span, preferred_span, confidence, history_id, created_at, updated_at)
             VALUES (?1, ?2, 0.95, ?3, ?4, ?4)",
            params![original_span, preferred_span, history_id, now],
        )?;
        Ok(Some(connection.last_insert_rowid()))
    }

    pub fn list_dictionary_candidates(&self) -> Result<Vec<DictionaryCandidate>, StorageError> {
        if !self.get_settings()?.history_enabled {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT id, original_span, preferred_span, confidence, history_id, created_at FROM dictionary_candidates ORDER BY created_at DESC")?;
        let rows = statement.query_map([], |row| {
            Ok(DictionaryCandidate {
                id: row.get(0)?,
                original_span: row.get(1)?,
                preferred_span: row.get(2)?,
                confidence: row.get(3)?,
                history_id: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn confirm_dictionary_candidate(
        &self,
        id: i64,
    ) -> Result<Option<DictionaryEntry>, StorageError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let candidate = transaction
            .query_row(
                "SELECT original_span, preferred_span FROM dictionary_candidates WHERE id = ?1",
                [id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((reading, surface)) = candidate else {
            return Ok(None);
        };
        let aliases = Vec::new();
        let entry = NewDictionaryEntry {
            reading: &reading,
            surface: &surface,
            category: None,
            aliases: &aliases,
            priority: 0,
            app_scope: None,
        };
        let existing = read_dictionary_entries(&transaction)?;
        let match_id = existing
            .iter()
            .find(|item| {
                item.surface.trim().to_lowercase() == surface.trim().to_lowercase()
                    && normalized_scope(item.app_scope.as_deref()).is_none()
            })
            .map(|item| item.id);
        if match_id.is_none() {
            insert_dictionary_entry(&transaction, &entry, "auto", None)?;
        }
        transaction.execute("INSERT OR IGNORE INTO dictionary_candidate_decisions(original_span, preferred_span, created_at) VALUES (?1, ?2, ?3)", params![reading, surface, Utc::now().to_rfc3339()])?;
        transaction.execute(
            "DELETE FROM dictionary_candidates WHERE original_span = ?1 AND preferred_span = ?2",
            params![reading, surface],
        )?;
        let result = read_dictionary_entries(&transaction)?
            .into_iter()
            .find(|item| {
                item.surface.trim().to_lowercase() == surface.trim().to_lowercase()
                    && normalized_scope(item.app_scope.as_deref()).is_none()
            });
        transaction.commit()?;
        Ok(result)
    }

    pub fn reject_dictionary_candidate(&self, id: i64) -> Result<bool, StorageError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let candidate = transaction
            .query_row(
                "SELECT original_span, preferred_span FROM dictionary_candidates WHERE id = ?1",
                [id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((original, preferred)) = candidate else {
            return Ok(false);
        };
        transaction.execute("INSERT OR IGNORE INTO dictionary_candidate_decisions(original_span, preferred_span, created_at) VALUES (?1, ?2, ?3)", params![original, preferred, Utc::now().to_rfc3339()])?;
        transaction.execute(
            "DELETE FROM dictionary_candidates WHERE original_span = ?1 AND preferred_span = ?2",
            params![original, preferred],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn dictionary_prompt_terms_for(
        &self,
        context: Option<&crate::types::AppContext>,
    ) -> Result<Vec<String>, StorageError> {
        let mut terms = Vec::new();
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT reading, surface, aliases, app_scope
             FROM dictionary_entries ORDER BY priority DESC, surface ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            if terms.len() >= 100 {
                break;
            }
            let (reading, surface, aliases, scope) = row?;
            if !scope_matches(scope.as_deref(), context) {
                continue;
            }
            let aliases = serde_json::from_str::<Vec<String>>(&aliases)?;
            let mut term = format!("{} => {}", reading.trim(), surface.trim());
            if !aliases.is_empty() {
                term.push_str(&format!(" (aliases: {})", aliases.join(", ")));
            }
            terms.push(term.chars().take(300).collect());
        }
        Ok(terms)
    }

    pub fn dictionary_prompt_terms(&self) -> Result<Vec<String>, StorageError> {
        self.dictionary_prompt_terms_for(None)
    }

    pub fn dictionary_asr_prompt_for(
        &self,
        context: Option<&crate::types::AppContext>,
        backend: &str,
    ) -> Result<Option<String>, StorageError> {
        let entries = self.list_dictionary()?;
        let mut terms = Vec::new();
        let mut length = 0;
        let budget = if backend == "faster-whisper" {
            200
        } else {
            1200
        };
        for entry in entries
            .into_iter()
            .filter(|entry| scope_matches(entry.app_scope.as_deref(), context))
        {
            if backend == "faster-whisper" {
                // Hotwords are plain terms, not the `reading => surface`
                // syntax used by prompt-based backends. Prefer the surface,
                // then aliases and manual readings within its small budget.
                for term in std::iter::once(entry.surface.as_str())
                    .chain(entry.aliases.iter().map(String::as_str))
                    .chain((entry.source == "manual").then_some(entry.reading.as_str()))
                {
                    let separator = if terms.is_empty() { 0 } else { 2 };
                    if length + term.chars().count() + separator <= budget {
                        length += term.chars().count() + separator;
                        terms.push(term.to_owned());
                    }
                }
            } else {
                let aliases = if entry.aliases.is_empty() {
                    String::new()
                } else {
                    format!(" (aliases: {})", entry.aliases.join(", "))
                };
                let term = if entry.source == "auto" {
                    format!("{}{}", entry.surface, aliases)
                } else {
                    format!("{} => {}{}", entry.reading, entry.surface, aliases)
                };
                let separator = if terms.is_empty() { 0 } else { 1 };
                if length + term.chars().count() + separator <= budget {
                    length += term.chars().count() + separator;
                    terms.push(term);
                }
            }
        }
        let separator = if backend == "faster-whisper" {
            ", "
        } else {
            "\n"
        };
        Ok((!terms.is_empty()).then(|| terms.join(separator)))
    }

    pub fn dictionary_correction_hints(
        &self,
        transcript: &str,
        context: Option<&crate::types::AppContext>,
    ) -> Result<Vec<String>, StorageError> {
        let transcript = transcript.to_lowercase();
        Ok(self
            .list_dictionary()?
            .into_iter()
            .filter(|entry| scope_matches(entry.app_scope.as_deref(), context))
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

#[derive(Debug, Clone)]
struct OwnedDictionaryEntry {
    reading: String,
    surface: String,
    category: Option<String>,
    aliases: Vec<String>,
    priority: i64,
    app_scope: Option<String>,
}

impl OwnedDictionaryEntry {
    fn as_new(&self) -> NewDictionaryEntry<'_> {
        NewDictionaryEntry {
            reading: &self.reading,
            surface: &self.surface,
            category: self.category.as_deref(),
            aliases: &self.aliases,
            priority: self.priority,
            app_scope: self.app_scope.as_deref(),
        }
    }
}

fn normalized_scope(scope: Option<&str>) -> Option<String> {
    scope
        .map(str::trim)
        .filter(|scope| !scope.is_empty() && *scope != "global")
        .map(ToOwned::to_owned)
}

fn validate_dictionary_entry(entry: &NewDictionaryEntry<'_>) -> Result<(), StorageError> {
    for (name, value, required) in [
        ("reading", entry.reading, true),
        ("surface", entry.surface, true),
        ("category", entry.category.unwrap_or(""), false),
    ] {
        let trimmed = value.trim();
        if (required && trimmed.is_empty())
            || trimmed.chars().count() > MAX_DICTIONARY_TEXT_CHARS
            || trimmed.chars().any(char::is_control)
        {
            return Err(StorageError::Validation(format!(
                "{name} is empty, too long, or contains control characters"
            )));
        }
    }
    crate::personalization::validate_dictionary_scope(entry.app_scope.map(str::trim))
        .map_err(|error| StorageError::Validation(error.into()))?;
    let mut aliases = std::collections::HashSet::new();
    for alias in entry.aliases {
        let alias = alias.trim();
        if alias.is_empty()
            || alias.chars().count() > MAX_DICTIONARY_TEXT_CHARS
            || alias.chars().any(char::is_control)
            || !aliases.insert(alias.to_lowercase())
        {
            return Err(StorageError::Validation(
                "aliases must be unique, non-empty, and valid".into(),
            ));
        }
    }
    Ok(())
}

fn validate_dictionary_collisions(
    existing: &[DictionaryEntry],
    entry: &NewDictionaryEntry<'_>,
    excluded_id: Option<i64>,
) -> Result<(), StorageError> {
    let scope = normalized_scope(entry.app_scope);
    let surface = entry.surface.trim().to_lowercase();
    let aliases = entry
        .aliases
        .iter()
        .map(|value| value.trim().to_lowercase())
        .collect::<Vec<_>>();
    for current in existing.iter().filter(|current| {
        Some(current.id) != excluded_id && normalized_scope(current.app_scope.as_deref()) == scope
    }) {
        let current_surface = current.surface.trim().to_lowercase();
        let current_aliases = current
            .aliases
            .iter()
            .map(|value| value.trim().to_lowercase())
            .collect::<Vec<_>>();
        if current_surface == surface {
            return Err(StorageError::Validation(
                "surface already exists in this scope".into(),
            ));
        }
        if current_aliases.iter().any(|value| value == &surface)
            || aliases.iter().any(|value| {
                value == &current_surface || current_aliases.iter().any(|current| current == value)
            })
        {
            return Err(StorageError::Validation(
                "surface or alias collides with another entry in this scope".into(),
            ));
        }
    }
    Ok(())
}

fn read_dictionary_entries<C: std::ops::Deref<Target = Connection>>(
    connection: &C,
) -> Result<Vec<DictionaryEntry>, StorageError> {
    let mut statement = connection.prepare(
        "SELECT id, reading, surface, category, aliases, priority, app_scope, source, created_at
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
            row.get::<_, String>(8)?,
        ))
    })?;
    rows.map(|row| {
        let (id, reading, surface, category, aliases, priority, app_scope, source, created_at) =
            row?;
        Ok(DictionaryEntry {
            id,
            reading,
            surface,
            category,
            aliases: serde_json::from_str(&aliases)?,
            priority,
            app_scope: normalized_scope(app_scope.as_deref()),
            source,
            created_at,
        })
    })
    .collect()
}

fn insert_dictionary_entry<C: std::ops::Deref<Target = Connection>>(
    connection: &C,
    entry: &NewDictionaryEntry<'_>,
    source: &str,
    excluded_id: Option<i64>,
) -> Result<i64, StorageError> {
    validate_dictionary_entry(entry)?;
    validate_dictionary_collisions(&read_dictionary_entries(connection)?, entry, excluded_id)?;
    let now = Utc::now().to_rfc3339();
    connection.execute(
        "INSERT INTO dictionary_entries(reading, surface, category, aliases, priority, app_scope, source, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        params![entry.reading.trim(), entry.surface.trim(), entry.category.map(str::trim).filter(|value| !value.is_empty()), serde_json::to_string(entry.aliases)?, entry.priority, normalized_scope(entry.app_scope), source, now],
    )?;
    Ok(connection.last_insert_rowid())
}

pub(crate) fn detect_dictionary_candidate(
    original: &str,
    corrected: &str,
) -> Option<(String, String)> {
    if original == corrected {
        return None;
    }
    let has_code_or_url = |value: &str| {
        let lower = value.to_ascii_lowercase();
        let dotted_identifier = value.as_bytes().windows(3).any(|window| {
            window[1] == b'.'
                && window[0].is_ascii_alphanumeric()
                && window[2].is_ascii_alphanumeric()
        });
        lower.contains("http://")
            || lower.contains("https://")
            || lower.contains("www.")
            || dotted_identifier
            || value.contains(['/', '\\'])
            || value.contains([
                '`', '{', '}', ';', '(', ')', '[', ']', '=', '<', '>', '#', '@', '$', '&', '|',
                '"', '\'', '+', '*', '%', '!', '^',
            ])
            || value.contains("::")
            || value.contains("->")
            || value.contains("=>")
    };
    if has_code_or_url(original) || has_code_or_url(corrected) {
        return None;
    }
    let original_chars = original.chars().collect::<Vec<_>>();
    let corrected_chars = corrected.chars().collect::<Vec<_>>();
    let mut prefix = 0;
    while prefix < original_chars.len()
        && prefix < corrected_chars.len()
        && original_chars[prefix] == corrected_chars[prefix]
    {
        prefix += 1;
    }
    let mut original_end = original_chars.len();
    let mut corrected_end = corrected_chars.len();
    while original_end > prefix
        && corrected_end > prefix
        && original_chars[original_end - 1] == corrected_chars[corrected_end - 1]
    {
        original_end -= 1;
        corrected_end -= 1;
    }
    // The shortest character diff can be a single letter or an empty span for
    // spacing changes. Expand both sides to complete adjacent terms.
    let is_term = |character: char| character.is_alphanumeric() || matches!(character, '-' | '_');
    let mut original_start = prefix;
    let mut preferred_start = prefix;
    while original_start > 0 && is_term(original_chars[original_start - 1]) {
        original_start -= 1;
    }
    while preferred_start > 0 && is_term(corrected_chars[preferred_start - 1]) {
        preferred_start -= 1;
    }
    while original_end < original_chars.len() && is_term(original_chars[original_end]) {
        original_end += 1;
    }
    while corrected_end < corrected_chars.len() && is_term(corrected_chars[corrected_end]) {
        corrected_end += 1;
    }
    let original_span = original_chars[original_start..original_end]
        .iter()
        .collect::<String>();
    let preferred_span = corrected_chars[preferred_start..corrected_end]
        .iter()
        .collect::<String>();
    let normalized = |value: &str| {
        value
            .chars()
            .filter(|character| !character.is_whitespace() && !matches!(character, '-' | '_'))
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let original_normalized = normalized(&original_span);
    let preferred_normalized = normalized(&preferred_span);
    let invalid = |value: &str| {
        value.trim().is_empty()
            || value.chars().count() > MAX_CANDIDATE_CHARS
            || value.contains(['\n', '\r'])
            || value.chars().any(char::is_control)
            || value
                .chars()
                .all(|character| character.is_ascii_digit() || matches!(character, ' ' | '-' | '_'))
    };
    if invalid(&original_span)
        || invalid(&preferred_span)
        || original_normalized.is_empty()
        || original_normalized != preferred_normalized
    {
        return None;
    }
    Some((original_span, preferred_span))
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
    fn disabled_correction_preserves_configuration() {
        let storage = Storage::in_memory().unwrap();
        let settings = Settings {
            text_correction_enabled: false,
            correction_provider: "gemini".into(),
            gemini_correction_model: "gemini-custom".into(),
            correction_remove_fillers: false,
            correction_auto_format: false,
            correction_instruction: "Keep technical terms unchanged.".into(),
            ..Settings::default()
        };

        storage.update_settings(&settings).unwrap();

        let restored = storage.get_settings().unwrap();
        assert!(!restored.text_correction_enabled);
        assert_eq!(restored.correction_provider, "gemini");
        assert_eq!(restored.gemini_correction_model, "gemini-custom");
        assert!(!restored.correction_remove_fillers);
        assert!(!restored.correction_auto_format);
        assert_eq!(
            restored.correction_instruction,
            "Keep technical terms unchanged."
        );
    }

    #[test]
    fn legacy_settings_receive_audio_enhancement_defaults() {
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("noiseSuppression");
        object.remove("inputGainPercent");
        object.remove("automaticGain");
        let settings: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(settings.noise_suppression, "medium");
        assert_eq!(settings.input_gain_percent, 100);
        assert!(settings.automatic_gain);
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

    #[test]
    fn history_round_trips_captured_app_category() {
        let storage = Storage::in_memory().unwrap();
        let mut value = item();
        value.app_category = Some("development");
        storage.add_history(&value).unwrap();
        assert_eq!(
            storage.list_history(10).unwrap()[0].app_category.as_deref(),
            Some("development")
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
    fn dictionary_prompt_terms_include_reading_surface_and_aliases() {
        let storage = Storage::in_memory().unwrap();
        let aliases = vec!["ChatGPT".into()];
        storage
            .add_dictionary_entry(&dictionary_entry(&aliases))
            .unwrap();
        let terms = storage.dictionary_prompt_terms().unwrap();
        assert_eq!(terms, vec!["おーぷんえーあい => OpenAI (aliases: ChatGPT)"]);
    }

    #[test]
    fn scoped_dictionary_terms_and_hints_include_matching_context_only() {
        let storage = Storage::in_memory().unwrap();
        let aliases = vec!["global-alias".into()];
        for (reading, surface, scope) in [
            ("global-alias", "Global", None),
            ("app-alias", "App", Some("app:code")),
            ("category-alias", "Category", Some("category:development")),
            ("other-alias", "Other", Some("app:other")),
        ] {
            storage
                .add_dictionary_entry(&NewDictionaryEntry {
                    reading,
                    surface,
                    category: None,
                    aliases: &aliases,
                    priority: 1,
                    app_scope: scope,
                })
                .unwrap();
        }
        let context = crate::types::AppContext {
            app_key: Some("code".into()),
            category: "development".into(),
        };
        let terms = storage.dictionary_prompt_terms_for(Some(&context)).unwrap();
        assert!(terms
            .iter()
            .any(|term| term.starts_with("global-alias => Global")));
        assert!(terms
            .iter()
            .any(|term| term.starts_with("app-alias => App")));
        assert!(terms
            .iter()
            .any(|term| term.starts_with("category-alias => Category")));
        assert!(!terms
            .iter()
            .any(|term| term.starts_with("other-alias => Other")));
        let hints = storage
            .dictionary_correction_hints(
                "global-alias app-alias category-alias other-alias",
                Some(&context),
            )
            .unwrap();
        assert!(hints.iter().any(|hint| hint.starts_with("Global<=")));
        assert!(hints.iter().any(|hint| hint.starts_with("App<=")));
        assert!(hints.iter().any(|hint| hint.starts_with("Category<=")));
        assert!(!hints.iter().any(|hint| hint.starts_with("Other<=")));
    }

    #[test]
    fn app_lookup_failure_uses_global_entries_and_records_no_category() {
        let storage = Storage::in_memory().unwrap();
        let aliases = Vec::new();
        for (reading, surface, scope) in [
            ("global-reading", "Global", None),
            ("other-reading", "OtherCategory", Some("category:other")),
            ("editor-reading", "EditorApp", Some("app:myeditor")),
        ] {
            storage
                .add_dictionary_entry(&NewDictionaryEntry {
                    reading,
                    surface,
                    category: None,
                    aliases: &aliases,
                    priority: 1,
                    app_scope: scope,
                })
                .unwrap();
        }

        let failed = crate::app_context::from_executable_path(None);
        assert_eq!(
            storage
                .dictionary_prompt_terms_for(failed.as_ref())
                .unwrap(),
            vec!["global-reading => Global".to_string()]
        );
        let hints = storage
            .dictionary_correction_hints(
                "global-reading other-reading editor-reading",
                failed.as_ref(),
            )
            .unwrap();
        assert_eq!(hints, vec!["Global<=global-reading".to_string()]);
        let mut value = item();
        value.app_category = crate::app_context::history_category(failed.as_ref());
        storage.add_history(&value).unwrap();
        assert_eq!(storage.list_history(10).unwrap()[0].app_category, None);

        let unclassified = crate::app_context::from_executable_path(Some(r"C:\Tools\MyEditor.exe"));
        let terms = storage
            .dictionary_prompt_terms_for(unclassified.as_ref())
            .unwrap();
        for surface in ["Global", "OtherCategory", "EditorApp"] {
            assert!(
                terms
                    .iter()
                    .any(|term| term.ends_with(&format!("=> {surface}"))),
                "missing {surface}"
            );
        }
        let mut value = item();
        value.app_category = crate::app_context::history_category(unclassified.as_ref());
        storage.add_history(&value).unwrap();
        let mut categories = storage
            .list_history(10)
            .unwrap()
            .into_iter()
            .map(|item| item.app_category)
            .collect::<Vec<_>>();
        categories.sort();
        assert_eq!(categories, vec![None, Some("other".to_string())]);
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
                .dictionary_correction_hints("Chat GPTを使います", None)
                .unwrap(),
            vec!["OpenAI<=Chat GPT"]
        );
        assert!(storage
            .dictionary_correction_hints("関係のない文章", None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn dictionary_correction_hints_follow_priority_order() {
        let storage = Storage::in_memory().unwrap();
        let low = vec!["low alias".to_owned()];
        let high = vec!["high alias".to_owned()];
        storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "low",
                surface: "Low",
                category: None,
                aliases: &low,
                priority: 1,
                app_scope: None,
            })
            .unwrap();
        storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "high",
                surface: "High",
                category: None,
                aliases: &high,
                priority: 9,
                app_scope: None,
            })
            .unwrap();
        assert_eq!(
            storage
                .dictionary_correction_hints("high alias and low alias", None)
                .unwrap(),
            vec!["High<=high alias", "Low<=low alias"]
        );
    }

    #[test]
    fn disabling_history_purges_existing_rows() {
        let storage = Storage::in_memory().unwrap();
        storage.add_history(&item()).unwrap();
        storage
            .add_dictionary_candidate_from_correction("open ai", "OpenAI", None)
            .unwrap();
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
        assert!(storage.list_dictionary_candidates().unwrap().is_empty());
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

    #[test]
    fn dictionary_update_keeps_id_source_scope_and_aliases() {
        let storage = Storage::in_memory().unwrap();
        let aliases = vec!["First alias".into()];
        let id = storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "read",
                surface: "Surface",
                category: Some("old"),
                aliases: &aliases,
                priority: 1,
                app_scope: Some("app:code"),
            })
            .unwrap();
        let replacement_aliases = vec!["Second alias".into()];
        assert!(storage
            .update_dictionary_entry(
                id,
                &NewDictionaryEntry {
                    reading: "new read",
                    surface: "New surface",
                    category: Some("new"),
                    aliases: &replacement_aliases,
                    priority: 9,
                    app_scope: Some("app:code"),
                }
            )
            .unwrap());
        assert_eq!(
            storage.list_dictionary().unwrap(),
            vec![DictionaryEntry {
                id,
                reading: "new read".into(),
                surface: "New surface".into(),
                category: Some("new".into()),
                aliases: replacement_aliases,
                priority: 9,
                app_scope: Some("app:code".into()),
                source: "manual".into(),
                created_at: storage.list_dictionary().unwrap()[0].created_at.clone(),
            }]
        );
    }

    #[test]
    fn dictionary_search_matches_every_field_and_source() {
        let storage = Storage::in_memory().unwrap();
        let aliases = vec!["alternate".into()];
        storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "spoken",
                surface: "Surface",
                category: Some("group"),
                aliases: &aliases,
                priority: 0,
                app_scope: Some("category:email"),
            })
            .unwrap();
        storage
            .add_dictionary_candidate_from_correction("auto value", "AutoValue", None)
            .unwrap();
        let candidate = storage.list_dictionary_candidates().unwrap().pop().unwrap();
        storage.confirm_dictionary_candidate(candidate.id).unwrap();
        for query in ["spoken", "surface", "group", "alternate", "email"] {
            assert_eq!(
                storage
                    .search_dictionary(Some(query), Some("manual"))
                    .unwrap()
                    .len(),
                1,
                "{query}"
            );
        }
        assert_eq!(
            storage.search_dictionary(None, Some("auto")).unwrap().len(),
            1
        );
        assert_eq!(
            storage
                .search_dictionary(None, Some("manual"))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn csv_import_quotes_and_rolls_back_invalid_or_colliding_rows() {
        let storage = Storage::in_memory().unwrap();
        let csv = "reading,surface,category,aliases,priority,app_scope\n\"read, ing\",Surface,group,\"one|two\",3,app:code\nnext,Other,,,0,\n";
        assert_eq!(storage.import_dictionary_csv(csv).unwrap(), 2);
        assert_eq!(storage.list_dictionary().unwrap().len(), 2);
        for invalid in [
            "reading,surface,category,aliases,priority,app_scope\nread,Surface,,,nope,\n",
            "reading,surface,category,aliases,priority,app_scope\nfresh,Fresh,,,0,\nother,Other,,fresh,0,\n",
            "reading,surface,category,aliases,priority,app_scope\nread,Surface,,,0,app:INVALID\n",
            "reading,surface,category,aliases,priority,app_scope\n,Surface,,,0,\n",
            "reading,surface,category,aliases,priority,app_scope\nread,Surface,,one||two,0,\n",
            "reading,surface,category,aliases,priority,app_scope\nread,Surface,,bad\u{0007},0,\n",
            "wrong,header\n",
        ] {
            assert!(storage.import_dictionary_csv(invalid).is_err());
            assert_eq!(storage.list_dictionary().unwrap().len(), 2);
        }
        assert!(storage
            .import_dictionary_csv(&"x".repeat(MAX_CSV_BYTES + 1))
            .is_err());
        assert!(storage
            .import_dictionary_csv(&format!(
                "reading,surface,category,aliases,priority,app_scope\n{},Surface,,,0,\n",
                "x".repeat(MAX_DICTIONARY_TEXT_CHARS + 1)
            ))
            .is_err());
        let too_many = format!(
            "reading,surface,category,aliases,priority,app_scope\n{}",
            (0..=MAX_CSV_ROWS)
                .map(|index| format!("r{index},s{index},,,0,\n"))
                .collect::<String>()
        );
        assert!(storage.import_dictionary_csv(&too_many).is_err());
    }

    #[test]
    fn dictionary_collisions_are_scope_specific_and_case_insensitive() {
        let storage = Storage::in_memory().unwrap();
        let aliases = vec!["Alias".into()];
        storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "one",
                surface: "Surface",
                category: None,
                aliases: &aliases,
                priority: 0,
                app_scope: None,
            })
            .unwrap();
        let no_aliases = Vec::new();
        assert!(storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "two",
                surface: "surface",
                category: None,
                aliases: &no_aliases,
                priority: 0,
                app_scope: Some("global")
            })
            .is_err());
        assert!(storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "two",
                surface: "Other",
                category: None,
                aliases: &["alias".into()],
                priority: 0,
                app_scope: None
            })
            .is_err());
        assert!(storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "two",
                surface: "Surface",
                category: None,
                aliases: &no_aliases,
                priority: 0,
                app_scope: Some("app:code")
            })
            .is_ok());
    }

    #[test]
    fn candidate_detection_is_narrow_and_confirmation_is_explicit() {
        assert_eq!(
            detect_dictionary_candidate("Use open ai now", "Use OpenAI now"),
            Some(("open ai".into(), "OpenAI".into()))
        );
        assert_eq!(
            detect_dictionary_candidate("hello world", "goodbye world"),
            None
        );
        for value in [
            "https://example.com",
            "See example.com",
            "foo/bar",
            "module.function",
            "foo + bar",
            "123-456",
            "foo::bar",
            "foo()",
            "line\nbreak",
        ] {
            assert_eq!(
                detect_dictionary_candidate(value, &value.to_uppercase()),
                None,
                "{value}"
            );
        }
        assert_eq!(detect_dictionary_candidate("foo + bar", "Foo+Bar"), None);
        assert_eq!(
            detect_dictionary_candidate("use Github", "use GitHub"),
            Some(("Github".into(), "GitHub".into()))
        );
        assert_eq!(
            detect_dictionary_candidate("Chat GPT", "ChatGPT"),
            Some(("Chat GPT".into(), "ChatGPT".into()))
        );
        assert_eq!(
            detect_dictionary_candidate("open AI", "OpenAI"),
            Some(("open AI".into(), "OpenAI".into()))
        );
        let storage = Storage::in_memory().unwrap();
        let id = storage
            .add_dictionary_candidate_from_correction("open ai", "OpenAI", None)
            .unwrap()
            .unwrap();
        assert!(storage.list_dictionary().unwrap().is_empty());
        let confirmed = storage.confirm_dictionary_candidate(id).unwrap().unwrap();
        assert_eq!(confirmed.source, "auto");
        let rejected = storage
            .add_dictionary_candidate_from_correction("chat gpt", "ChatGPT", None)
            .unwrap()
            .unwrap();
        assert!(storage.reject_dictionary_candidate(rejected).unwrap());
        assert_eq!(storage.list_dictionary().unwrap().len(), 1);
        let disabled = Settings {
            history_enabled: false,
            ..Settings::default()
        };
        storage.update_settings(&disabled).unwrap();
        assert_eq!(
            storage
                .add_dictionary_candidate_from_correction("open ai", "OpenAI", None)
                .unwrap(),
            None
        );
    }

    #[test]
    fn confirming_candidate_preserves_manual_entry_and_suppresses_repeats() {
        let storage = Storage::in_memory().unwrap();
        let aliases = vec!["Existing alias".into()];
        let manual = storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "正しい読み",
                surface: "GitHub",
                category: Some("service"),
                aliases: &aliases,
                priority: 9,
                app_scope: None,
            })
            .unwrap();
        let candidate_id = storage
            .add_dictionary_candidate_from_correction("Github", "GitHub", None)
            .unwrap();
        assert_eq!(candidate_id, None);
        // A candidate generated before a manual entry was created must also be safe.
        let pending = storage
            .add_dictionary_candidate_from_correction("open ai", "OpenAI", None)
            .unwrap()
            .unwrap();
        let open_aliases = vec!["Other alias".into()];
        let manual_open = storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "おーぷんえーあい",
                surface: "OpenAI",
                category: Some("service"),
                aliases: &open_aliases,
                priority: 9,
                app_scope: None,
            })
            .unwrap();
        let confirmed = storage
            .confirm_dictionary_candidate(pending)
            .unwrap()
            .unwrap();
        assert_eq!(confirmed.id, manual_open);
        assert_eq!(confirmed.reading, "おーぷんえーあい");
        assert_eq!(confirmed.source, "manual");
        assert_eq!(confirmed.aliases, open_aliases);
        assert_eq!(confirmed.priority, 9);
        assert_eq!(storage.list_dictionary().unwrap().len(), 2);
        assert!(storage
            .list_dictionary()
            .unwrap()
            .iter()
            .any(|entry| entry.id == manual && entry.source == "manual"));
        assert_eq!(
            storage
                .add_dictionary_candidate_from_correction("open ai", "OpenAI", None)
                .unwrap(),
            None
        );

        let rejected = storage
            .add_dictionary_candidate_from_correction("chat gpt", "ChatGPT", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            storage
                .add_dictionary_candidate_from_correction("chat gpt", "ChatGPT", None)
                .unwrap(),
            None
        );
        assert!(storage.reject_dictionary_candidate(rejected).unwrap());
        assert_eq!(
            storage
                .add_dictionary_candidate_from_correction("chat gpt", "ChatGPT", None)
                .unwrap(),
            None
        );
    }

    #[test]
    fn asr_prompt_is_backend_specific_and_bounded() {
        let storage = Storage::in_memory().unwrap();
        storage
            .add_dictionary_entry(&dictionary_entry(&["ChatGPT".into()]))
            .unwrap();
        let hotwords = storage
            .dictionary_asr_prompt_for(None, "faster-whisper")
            .unwrap()
            .unwrap();
        assert_eq!(hotwords, "OpenAI, ChatGPT, おーぷんえーあい");
        let prompt = storage
            .dictionary_asr_prompt_for(None, "openai-compatible")
            .unwrap()
            .unwrap();
        assert!(prompt.contains("おーぷんえーあい => OpenAI"));
        assert!(prompt.contains("ChatGPT"));
    }

    #[test]
    fn history_policy_expires_pending_and_decided_candidate_text() {
        let storage = Storage::in_memory().unwrap();
        let pending = storage
            .add_dictionary_candidate_from_correction("use Github", "use GitHub", None)
            .unwrap()
            .unwrap();
        let rejected = storage
            .add_dictionary_candidate_from_correction("open ai", "OpenAI", None)
            .unwrap()
            .unwrap();
        assert!(storage.reject_dictionary_candidate(rejected).unwrap());
        let connection = storage.connection().unwrap();
        connection.execute("UPDATE dictionary_candidates SET created_at = '2000-01-01T00:00:00Z' WHERE id = ?1", [pending]).unwrap();
        connection
            .execute(
                "UPDATE dictionary_candidate_decisions SET created_at = '2000-01-01T00:00:00Z'",
                [],
            )
            .unwrap();
        drop(connection);
        let settings = storage.get_settings().unwrap();
        storage.apply_history_policy(&settings, &settings).unwrap();
        let connection = storage.connection().unwrap();
        let pending_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM dictionary_candidates", [], |row| {
                row.get(0)
            })
            .unwrap();
        let decision_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM dictionary_candidate_decisions",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((pending_count, decision_count), (0, 0));
        drop(connection);
        assert!(storage
            .add_dictionary_candidate_from_correction("open ai", "OpenAI", None)
            .unwrap()
            .is_some());
    }

    #[test]
    fn disabling_history_atomically_prevents_new_candidate_text() {
        let storage = Storage::in_memory().unwrap();
        storage
            .add_dictionary_candidate_from_correction("open ai", "OpenAI", None)
            .unwrap();
        let mut settings = storage.get_settings().unwrap();
        settings.history_enabled = false;
        storage
            .update_settings_with_history_policy(&settings)
            .unwrap();
        assert_eq!(
            storage
                .add_dictionary_candidate_from_correction("chat gpt", "ChatGPT", None)
                .unwrap(),
            None
        );
        let connection = storage.connection().unwrap();
        let pending: i64 = connection
            .query_row("SELECT COUNT(*) FROM dictionary_candidates", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(pending, 0);
    }
}
