use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

#[cfg(test)]
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

use crate::types::{
    DictionaryCandidate, DictionaryEntry, HistoryAudioPayload, HistoryFilter, HistoryItem,
    HistoryRetention, NewDictionaryEntry, NewHistoryItem, Settings,
};

const MAX_HISTORY_AUDIO_BYTES: u64 = 50 * 1024 * 1024;
#[cfg(test)]
static AUDIO_NONCE: AtomicU64 = AtomicU64::new(1);
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
    #[error("history audio operation failed")]
    Io(#[from] io::Error),
    #[error("invalid history audio filename")]
    InvalidAudioFilename,
    #[error("history audio is too large")]
    AudioTooLarge,
    #[error("dictionary validation failed: {0}")]
    Validation(String),
    #[error("CSV parsing failed")]
    Csv(#[from] csv::Error),
}

pub struct Storage {
    connection: Mutex<Connection>,
    history_audio_dir: PathBuf,
    settings_writes: Mutex<()>,
}

/// Builds without the V2 `shortcuts` field read one scalar chord per mode.
/// Mirroring each mode's primary chord keeps those chords after a downgrade;
/// an older build that rewrites settings drops `shortcuts`, and the next
/// migration rebuilds it from these keys.
const LEGACY_SHORTCUT_KEYS: [&str; 4] = [
    "hotkey",
    "voiceTranslateHotkey",
    "askHotkey",
    "speakToEditHotkey",
];

fn legacy_shortcut_primaries(
    shortcuts: &crate::types::VoiceShortcuts,
) -> Result<[&str; 4], StorageError> {
    fn primary(chords: &[String]) -> Result<&str, StorageError> {
        chords.first().map(String::as_str).ok_or_else(|| {
            StorageError::InvalidSettings(serde_json::Error::io(io::Error::new(
                io::ErrorKind::InvalidData,
                "voice mode shortcut list is empty",
            )))
        })
    }
    Ok([
        primary(&shortcuts.dictate)?,
        primary(&shortcuts.translate)?,
        primary(&shortcuts.ask)?,
        primary(&shortcuts.edit)?,
    ])
}

/// Writes the legacy scalar keys; returns whether any value changed.
fn mirror_legacy_shortcut_keys(
    value: &mut serde_json::Value,
    shortcuts: &crate::types::VoiceShortcuts,
) -> Result<bool, StorageError> {
    let mut changed = false;
    for (key, primary) in LEGACY_SHORTCUT_KEYS
        .into_iter()
        .zip(legacy_shortcut_primaries(shortcuts)?)
    {
        if value[key] != primary {
            value[key] = primary.into();
            changed = true;
        }
    }
    Ok(changed)
}

fn serialized_settings(settings: &Settings) -> Result<String, StorageError> {
    let mut value = serde_json::to_value(settings)?;
    mirror_legacy_shortcut_keys(&mut value, &settings.shortcuts)?;
    Ok(serde_json::to_string(&value)?)
}

struct StagedHistoryAudio<'a> {
    filename: String,
    path: PathBuf,
    armed: bool,
    storage: &'a Storage,
}

impl StagedHistoryAudio<'_> {
    fn filename(&self) -> &str {
        &self.filename
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagedHistoryAudio<'_> {
    fn drop(&mut self) {
        if self.armed {
            match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {
                    // The finalized path is DB-owned-shaped but was never
                    // committed. Queue a best-effort retry without panicking
                    // or taking another long-lived lock during unwinding.
                    if self.storage.audio_path(&self.filename).is_ok() {
                        if let Ok(connection) = self.storage.connection() {
                            let _ = connection.execute(
                                "INSERT OR IGNORE INTO pending_audio_deletions(filename, created_at) VALUES (?1, ?2)",
                                params![self.filename, Utc::now().to_rfc3339()],
                            );
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
fn random_audio_stem() -> String {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = AUDIO_NONCE.fetch_add(1, Ordering::Relaxed);
    format!("{time:032x}{nonce:016x}")
}

fn random_history_audio_stem() -> Result<String, StorageError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| io::Error::other(format!("secure random generation failed: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let connection = Connection::open(path)?;
        let storage = Self::with_connection(
            connection,
            path.parent()
                .unwrap_or_else(|| Path::new("."))
                .join("history-audio"),
        );
        storage.migrate()?;
        storage.reconcile_history_audio()?;
        Ok(storage)
    }

    #[cfg(test)]
    pub(crate) fn in_memory() -> Result<Self, StorageError> {
        let storage = Self::with_connection(
            Connection::open_in_memory()?,
            std::env::temp_dir().join(format!(
                "local-voice-history-test-{}-{}",
                std::process::id(),
                AUDIO_NONCE.fetch_add(1, Ordering::Relaxed)
            )),
        );
        storage.migrate()?;
        Ok(storage)
    }

    fn with_connection(connection: Connection, history_audio_dir: PathBuf) -> Self {
        Self {
            connection: Mutex::new(connection),
            history_audio_dir,
            settings_writes: Mutex::new(()),
        }
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, StorageError> {
        self.connection.lock().map_err(|_| StorageError::Lock)
    }

    /// Serializes settings read-modify-write sequences. Every writer holds this
    /// from reading the settings its change is based on until the result is
    /// written, so concurrent writers cannot revert each other's changes.
    pub fn lock_settings_writes(&self) -> Result<MutexGuard<'_, ()>, StorageError> {
        self.settings_writes.lock().map_err(|_| StorageError::Lock)
    }

    fn migrate(&self) -> Result<(), StorageError> {
        let connection = self.connection()?;
        connection.execute_batch(
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
               source_text TEXT,
               instruction_text TEXT,
               action_kind TEXT,
               search_site TEXT,
               mode TEXT NOT NULL,
               asr_provider TEXT NOT NULL,
               llm_provider TEXT,
               target_language TEXT,
               app_category TEXT,
               duration_ms INTEGER,
               latency_ms INTEGER,
               created_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_history_created_at ON dictation_history(created_at DESC);
             CREATE TABLE IF NOT EXISTS pending_audio_deletions (
               filename TEXT PRIMARY KEY NOT NULL,
               created_at TEXT NOT NULL
             );
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
        let has_target_language: bool = connection.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM pragma_table_info('dictation_history')
               WHERE name = 'target_language'
             )",
            [],
            |row| row.get(0),
        )?;
        if !has_target_language {
            connection.execute(
                "ALTER TABLE dictation_history ADD COLUMN target_language TEXT",
                [],
            )?;
        }
        for column in [
            "source_text",
            "instruction_text",
            "action_kind",
            "search_site",
            "insertion_result",
            "insertion_detail",
        ] {
            let exists: bool = connection.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM pragma_table_info('dictation_history')
                   WHERE name = ?1
                 )",
                [column],
                |row| row.get(0),
            )?;
            if !exists {
                connection.execute(
                    &format!("ALTER TABLE dictation_history ADD COLUMN {column} TEXT"),
                    [],
                )?;
            }
        }
        for (column, definition) in [
            ("audio_filename", "TEXT"),
            (
                "retry_of_id",
                "INTEGER REFERENCES dictation_history(id) ON DELETE SET NULL",
            ),
        ] {
            let exists: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('dictation_history') WHERE name = ?1)",
                [column],
                |row| row.get(0),
            )?;
            if !exists {
                connection.execute(
                    &format!("ALTER TABLE dictation_history ADD COLUMN {column} {definition}"),
                    [],
                )?;
            }
        }
        // A legacy/corrupt database may have shared a filename before this
        // ownership invariant existed. Preserve the oldest association and
        // clear later duplicates so the new partial unique index can migrate.
        connection.execute(
            "UPDATE dictation_history SET audio_filename = NULL
             WHERE audio_filename IS NOT NULL AND id NOT IN (
               SELECT MIN(id) FROM dictation_history
               WHERE audio_filename IS NOT NULL GROUP BY audio_filename
             )",
            [],
        )?;
        connection.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_history_audio_filename
             ON dictation_history(audio_filename) WHERE audio_filename IS NOT NULL",
            [],
        )?;
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
        let Some(raw) = stored else {
            return Ok(Settings::default());
        };
        let mut value: serde_json::Value = serde_json::from_str(&raw)?;
        let object = value.as_object_mut().ok_or_else(|| {
            serde_json::Error::io(io::Error::new(
                io::ErrorKind::InvalidData,
                "settings must be an object",
            ))
        })?;
        let legacy_history = !object.contains_key("historyRetention");
        if legacy_history {
            let enabled = object
                .remove("historyEnabled")
                .and_then(|value| value.as_bool())
                .unwrap_or(true);
            let days = object
                .remove("historyRetentionDays")
                .and_then(|value| value.as_u64())
                .unwrap_or(30);
            let retention = if !enabled {
                "never"
            } else if days < 7 {
                "24_hours"
            } else if days < 30 {
                "one_week"
            } else if days < 365 {
                "one_month"
            } else {
                "one_year"
            };
            object.insert("historyRetention".into(), retention.into());
        }
        let legacy_shortcuts = !object.contains_key("shortcuts");
        if legacy_shortcuts {
            let scalar = |name: &str, fallback: &str| {
                object
                    .get(name)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(fallback)
                    .to_owned()
            };
            object.insert(
                "shortcuts".into(),
                serde_json::json!({
                    "dictate": [scalar("hotkey", "Ctrl+Shift+Space")],
                    "translate": [scalar("voiceTranslateHotkey", "Ctrl+Shift+Y")],
                    "ask": [scalar("askHotkey", "Ctrl+Shift+A")],
                    "edit": [scalar("speakToEditHotkey", "Ctrl+Shift+E")],
                }),
            );
        }
        let mut settings: Settings = serde_json::from_value(value.clone())?;
        let defaults = crate::types::VoiceShortcuts::default();
        let mut recovered_empty_shortcuts = false;
        for (key, saved, fallback) in [
            ("dictate", &mut settings.shortcuts.dictate, defaults.dictate),
            (
                "translate",
                &mut settings.shortcuts.translate,
                defaults.translate,
            ),
            ("ask", &mut settings.shortcuts.ask, defaults.ask),
            ("edit", &mut settings.shortcuts.edit, defaults.edit),
        ] {
            if saved.is_empty() {
                *saved = fallback;
                value["shortcuts"][key] = serde_json::to_value(&*saved)?;
                recovered_empty_shortcuts = true;
            }
        }
        crate::shortcuts::Routes::parse_saved(&settings).map_err(|message| {
            StorageError::InvalidSettings(serde_json::Error::io(io::Error::new(
                io::ErrorKind::InvalidData,
                message,
            )))
        })?;
        // V2 `shortcuts` is canonical; the scalar keys only mirror primaries.
        let legacy_keys_changed = mirror_legacy_shortcut_keys(&mut value, &settings.shortcuts)?;
        if legacy_history || legacy_shortcuts || legacy_keys_changed || recovered_empty_shortcuts {
            // Persist the normalized V2 retention value, but keep fields that
            // this version does not own. A migration must not erase a newer
            // client's unrelated setting merely because it encountered an
            // older retention representation.
            self.connection()?.execute(
                "INSERT INTO settings(key, value, updated_at) VALUES('app_settings', ?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![serde_json::to_string(&value)?, Utc::now().to_rfc3339()],
            )?;
        }
        Ok(settings)
    }

    pub fn update_settings(&self, settings: &Settings) -> Result<(), StorageError> {
        let value = serialized_settings(settings)?;
        self.connection()?.execute(
            "INSERT INTO settings(key, value, updated_at) VALUES('app_settings', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![value, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Persist settings and apply their retention policy as one database
    /// transaction. Files are handled only after the commit, so a settings
    /// write failure cannot erase existing History.
    pub fn update_settings_and_apply_history_policy(
        &self,
        settings: &Settings,
    ) -> Result<(), StorageError> {
        let value = serialized_settings(settings)?;
        let cutoff = match settings.history_retention {
            HistoryRetention::Never => None,
            HistoryRetention::Forever => {
                self.update_settings(settings)?;
                let _ = self.retry_pending_audio_deletions();
                return Ok(());
            }
            HistoryRetention::TwentyFourHours => {
                Some((Utc::now() - chrono::Duration::days(1)).to_rfc3339())
            }
            HistoryRetention::OneWeek => {
                Some((Utc::now() - chrono::Duration::days(7)).to_rfc3339())
            }
            HistoryRetention::OneMonth => {
                Some((Utc::now() - chrono::Duration::days(30)).to_rfc3339())
            }
            HistoryRetention::OneYear => {
                Some((Utc::now() - chrono::Duration::days(365)).to_rfc3339())
            }
        };
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO settings(key, value, updated_at) VALUES('app_settings', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![value, Utc::now().to_rfc3339()],
        )?;
        let removed = {
            let mut statement = transaction.prepare(
                "SELECT id, audio_filename FROM dictation_history WHERE ?1 IS NULL OR created_at < ?1",
            )?;
            let rows = statement
                .query_map(params![cutoff], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for (_, filename) in &removed {
            if let Some(filename) = filename {
                transaction.execute(
                    "INSERT OR IGNORE INTO pending_audio_deletions(filename, created_at) VALUES (?1, ?2)",
                    params![filename, Utc::now().to_rfc3339()],
                )?;
            }
        }
        for (id, _) in &removed {
            transaction.execute("DELETE FROM dictation_history WHERE id = ?1", [id])?;
        }
        purge_dictionary_candidates_before(&transaction, cutoff.as_deref())?;
        transaction.commit()?;
        drop(connection);
        let _ = self.retry_pending_audio_deletions();
        Ok(())
    }

    /// Enforce the stored retention without writing settings. A settings
    /// change must use `update_settings_and_apply_history_policy` instead, so
    /// the purge and the settings write cannot be split.
    fn apply_history_policy(&self, settings: &Settings) -> Result<(), StorageError> {
        match settings.history_retention {
            HistoryRetention::Never => self.delete_history_matching(None),
            HistoryRetention::Forever => {
                let _ = self.retry_pending_audio_deletions();
                Ok(())
            }
            retention => self.delete_history_matching(retention.days()),
        }
    }

    pub fn enforce_current_history_policy(&self) -> Result<(), StorageError> {
        let settings = self.get_settings()?;
        self.apply_history_policy(&settings)
    }

    #[cfg(test)]
    pub fn add_history(&self, item: &NewHistoryItem<'_>) -> Result<bool, StorageError> {
        self.add_history_with_audio(item, None)
    }

    #[cfg(test)]
    pub fn add_history_with_audio(
        &self,
        item: &NewHistoryItem<'_>,
        source_audio: Option<&Path>,
    ) -> Result<bool, StorageError> {
        self.add_history_with_audio_report(item, source_audio)
            .map(|(saved, _)| saved)
    }

    pub fn add_history_with_audio_report(
        &self,
        item: &NewHistoryItem<'_>,
        source_audio: Option<&Path>,
    ) -> Result<(bool, bool), StorageError> {
        let settings = self.get_settings()?;
        if settings.history_retention == HistoryRetention::Never {
            return Ok((false, false));
        }
        // Prune before insertion. After a row/file association commits, this
        // method must not turn cleanup trouble into a false command failure.
        self.apply_history_policy(&settings)?;
        let mut audio_stage_failed = false;
        let mut staged_audio = if !settings.delete_audio_after_processing {
            source_audio.and_then(|path| match self.stage_history_audio(path) {
                Ok(audio) => Some(audio),
                Err(_) => {
                    audio_stage_failed = true;
                    None
                }
            })
        } else {
            None
        };
        let mut connection = self.connection()?;
        // Serialize the final privacy decision with the insert. Settings can
        // change while the WAV is being staged, so the earlier snapshot alone
        // cannot decide whether this row or its audio may be retained.
        let current_raw: Option<String> = connection
            .query_row(
                "SELECT value FROM settings WHERE key = 'app_settings'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let current_settings = current_raw
            .as_deref()
            .map(serde_json::from_str::<Settings>)
            .transpose()?
            .unwrap_or_default();
        if current_settings.history_retention == HistoryRetention::Never {
            return Ok((false, false));
        }
        if !current_settings.delete_audio_after_processing
            && staged_audio.is_none()
            && source_audio.is_some()
            && settings.delete_audio_after_processing
        {
            staged_audio = source_audio.and_then(|path| match self.stage_history_audio(path) {
                Ok(audio) => Some(audio),
                Err(_) => {
                    audio_stage_failed = true;
                    None
                }
            });
        }
        let audio_filename = if current_settings.delete_audio_after_processing {
            None
        } else {
            staged_audio.as_ref().map(|audio| audio.filename())
        };
        let retained_audio = audio_filename.is_some();
        let transaction = connection.transaction()?;
        let insert = transaction.execute(
            "INSERT INTO dictation_history(
               transcript_text, processed_text, source_text, instruction_text, action_kind, search_site, mode,
               asr_provider, llm_provider, target_language, app_category, duration_ms,
               latency_ms, created_at, audio_filename, retry_of_id, insertion_result,
               insertion_detail
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                      CASE WHEN EXISTS (SELECT 1 FROM dictation_history WHERE id = ?16) THEN ?16 ELSE NULL END,
                      ?17, ?18)",
            params![
                item.transcript_text,
                item.processed_text,
                item.source_text,
                item.instruction_text,
                item.action_kind,
                item.search_site,
                item.mode,
                item.asr_provider,
                item.llm_provider,
                item.target_language,
                item.app_category,
                item.duration_ms,
                item.latency_ms,
                Utc::now().to_rfc3339(),
                audio_filename,
                item.retry_of_id,
                item.insertion_result,
                item.insertion_detail,
            ],
        );
        if let Err(error) = insert {
            return Err(error.into());
        }
        if let Err(error) = transaction.commit() {
            return Err(error.into());
        }
        if retained_audio {
            if let Some(audio) = &mut staged_audio {
                audio.disarm();
            }
        }
        drop(connection);
        Ok((
            true,
            audio_stage_failed && !current_settings.delete_audio_after_processing,
        ))
    }

    pub fn list_history(
        &self,
        filter: HistoryFilter,
        limit: u32,
    ) -> Result<Vec<HistoryItem>, StorageError> {
        let settings = self.get_settings()?;
        if settings.history_retention == HistoryRetention::Never {
            return Ok(Vec::new());
        }
        self.apply_history_policy(&settings)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, transcript_text, processed_text, mode, asr_provider, llm_provider,
                    target_language, app_category, duration_ms, latency_ms, created_at,
                    source_text, instruction_text, action_kind, search_site,
                    audio_filename IS NOT NULL, retry_of_id, insertion_result, insertion_detail
             FROM dictation_history
             WHERE ?1 = 'all' OR
               (?1 = 'dictate' AND mode IN ('faithful', 'ai_corrected', 'faithful_fallback')) OR
               (?1 = 'translate' AND mode = 'translate') OR
               (?1 = 'edit' AND mode = 'edit') OR
               (?1 = 'ask' AND mode = 'ask')
             ORDER BY created_at DESC LIMIT ?2",
        )?;
        let filter = match filter {
            HistoryFilter::All => "all",
            HistoryFilter::Dictate => "dictate",
            HistoryFilter::Translate => "translate",
            HistoryFilter::Edit => "edit",
            HistoryFilter::Ask => "ask",
        };
        let rows = statement.query_map(params![filter, limit.min(500)], |row| {
            Ok(HistoryItem {
                id: row.get(0)?,
                transcript_text: row.get(1)?,
                processed_text: row.get(2)?,
                source_text: row.get(11)?,
                instruction_text: row.get(12)?,
                action_kind: row.get(13)?,
                search_site: row.get(14)?,
                mode: row.get(3)?,
                asr_provider: row.get(4)?,
                llm_provider: row.get(5)?,
                target_language: row.get(6)?,
                app_category: row.get(7)?,
                duration_ms: row.get(8)?,
                latency_ms: row.get(9)?,
                created_at: row.get(10)?,
                has_audio: row.get(15)?,
                retry_of_id: row.get(16)?,
                insertion_result: row.get(17)?,
                insertion_detail: row.get(18)?,
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

    pub fn history_item(&self, id: i64) -> Result<Option<HistoryItem>, StorageError> {
        self.connection()?
            .query_row(
                "SELECT id, transcript_text, processed_text, mode, asr_provider, llm_provider,
                    target_language, app_category, duration_ms, latency_ms, created_at,
                    source_text, instruction_text, action_kind, search_site,
                    audio_filename IS NOT NULL, retry_of_id, insertion_result, insertion_detail
             FROM dictation_history WHERE id = ?1",
                [id],
                |row| {
                    Ok(HistoryItem {
                        id: row.get(0)?,
                        transcript_text: row.get(1)?,
                        processed_text: row.get(2)?,
                        mode: row.get(3)?,
                        asr_provider: row.get(4)?,
                        llm_provider: row.get(5)?,
                        target_language: row.get(6)?,
                        app_category: row.get(7)?,
                        duration_ms: row.get(8)?,
                        latency_ms: row.get(9)?,
                        created_at: row.get(10)?,
                        source_text: row.get(11)?,
                        instruction_text: row.get(12)?,
                        action_kind: row.get(13)?,
                        search_site: row.get(14)?,
                        has_audio: row.get(15)?,
                        retry_of_id: row.get(16)?,
                        insertion_result: row.get(17)?,
                        insertion_detail: row.get(18)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn delete_history(&self, id: i64) -> Result<bool, StorageError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let filename: Option<String> = transaction
            .query_row(
                "SELECT audio_filename FROM dictation_history WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        if let Some(filename) = &filename {
            transaction.execute(
                "INSERT OR IGNORE INTO pending_audio_deletions(filename, created_at) VALUES (?1, ?2)",
                params![filename, Utc::now().to_rfc3339()],
            )?;
        }
        let changed =
            transaction.execute("DELETE FROM dictation_history WHERE id = ?1", [id])? != 0;
        transaction.commit()?;
        drop(connection);
        let _ = self.retry_pending_audio_deletions();
        Ok(changed)
    }

    pub fn delete_all_history(&self) -> Result<u64, StorageError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let filenames = {
            let mut statement = transaction.prepare(
                "SELECT audio_filename FROM dictation_history WHERE audio_filename IS NOT NULL",
            )?;
            let values = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            values
        };
        for filename in &filenames {
            transaction.execute(
                "INSERT OR IGNORE INTO pending_audio_deletions(filename, created_at) VALUES (?1, ?2)",
                params![filename, Utc::now().to_rfc3339()],
            )?;
        }
        let changed = transaction.execute("DELETE FROM dictation_history", [])? as u64;
        transaction.commit()?;
        drop(connection);
        let _ = self.retry_pending_audio_deletions();
        Ok(changed)
    }

    pub fn history_audio(&self, id: i64) -> Result<Option<HistoryAudioPayload>, StorageError> {
        let filename: Option<String> = self
            .connection()?
            .query_row(
                "SELECT audio_filename FROM dictation_history WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(filename) = filename else {
            return Ok(None);
        };
        let path = self.audio_path(&filename)?;
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.connection()?.execute(
                    "UPDATE dictation_history SET audio_filename = NULL WHERE id = ?1",
                    [id],
                )?;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.len() > MAX_HISTORY_AUDIO_BYTES {
            return Err(StorageError::AudioTooLarge);
        }
        let bytes = fs::read(path)?;
        if bytes.len() as u64 > MAX_HISTORY_AUDIO_BYTES {
            return Err(StorageError::AudioTooLarge);
        }
        Ok(Some(HistoryAudioPayload {
            bytes,
            filename: format!("local-voice-history-{id}.wav"),
            mime_type: "audio/wav".into(),
        }))
    }

    pub fn copy_history_audio_for_retry(&self, id: i64) -> Result<PathBuf, StorageError> {
        // Keep the storage lock through the copy. Delete/retention use the
        // same lock before unlinking, so the source remains owned until the
        // guarded temporary copy is complete.
        let connection = self.connection()?;
        let filename: Option<String> = connection
            .query_row(
                "SELECT audio_filename FROM dictation_history WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let filename = filename.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "history audio is unavailable")
        })?;
        let source = self.audio_path(&filename)?;
        let metadata = fs::metadata(&source)?;
        if metadata.len() > MAX_HISTORY_AUDIO_BYTES {
            return Err(StorageError::AudioTooLarge);
        }
        let target = std::env::temp_dir().join(format!(
            "local-ai-voice-retry-{}-{}.wav",
            std::process::id(),
            random_history_audio_stem()?
        ));
        if let Err(error) = fs::copy(source, &target) {
            let _ = fs::remove_file(&target);
            return Err(error.into());
        }
        drop(connection);
        Ok(target)
    }

    fn stage_history_audio(&self, source: &Path) -> Result<StagedHistoryAudio<'_>, StorageError> {
        fs::create_dir_all(&self.history_audio_dir)?;
        let metadata = fs::metadata(source)?;
        if metadata.len() > MAX_HISTORY_AUDIO_BYTES {
            return Err(StorageError::AudioTooLarge);
        }
        let filename = format!("{}.wav", random_history_audio_stem()?);
        let final_path = self.audio_path(&filename)?;
        let staged = self.history_audio_dir.join(format!(".{filename}.stage"));
        if let Err(error) = fs::copy(source, &staged) {
            let _ = fs::remove_file(&staged);
            return Err(error.into());
        }
        let file = match fs::OpenOptions::new().write(true).open(&staged) {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_file(&staged);
                return Err(error.into());
            }
        };
        if let Err(error) = file.sync_all() {
            let _ = fs::remove_file(&staged);
            return Err(error.into());
        }
        if let Err(error) = fs::rename(&staged, &final_path) {
            let _ = fs::remove_file(&staged);
            return Err(error.into());
        }
        Ok(StagedHistoryAudio {
            filename,
            path: final_path,
            armed: true,
            storage: self,
        })
    }

    fn audio_path(&self, filename: &str) -> Result<PathBuf, StorageError> {
        let path = Path::new(filename);
        let stem = filename.strip_suffix(".wav").unwrap_or_default();
        if filename.is_empty()
            || path.is_absolute()
            || path.components().count() != 1
            || path.extension().and_then(|value| value.to_str()) != Some("wav")
            || stem.len() != 32
            || !stem.chars().all(|character| character.is_ascii_hexdigit())
            || filename.chars().any(|character| character.is_control())
        {
            return Err(StorageError::InvalidAudioFilename);
        }
        Ok(self.history_audio_dir.join(filename))
    }

    fn remove_or_queue_audio(&self, filename: &str) -> Result<(), StorageError> {
        let path = self.audio_path(filename)?;
        match fs::remove_file(path) {
            Ok(()) => {
                self.connection()?.execute(
                    "DELETE FROM pending_audio_deletions WHERE filename = ?1",
                    [filename],
                )?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.connection()?.execute(
                    "DELETE FROM pending_audio_deletions WHERE filename = ?1",
                    [filename],
                )?;
            }
            Err(_) => {
                self.connection()?.execute(
                "INSERT OR IGNORE INTO pending_audio_deletions(filename, created_at) VALUES (?1, ?2)",
                params![filename, Utc::now().to_rfc3339()],
            )?;
            }
        }
        Ok(())
    }

    pub fn retry_pending_audio_deletions(&self) -> Result<(), StorageError> {
        let filenames = {
            let connection = self.connection()?;
            let mut statement =
                connection.prepare("SELECT filename FROM pending_audio_deletions")?;
            let values = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            values
        };
        for filename in filenames {
            self.remove_or_queue_audio(&filename)?;
        }
        Ok(())
    }

    pub fn reconcile_history_audio(&self) -> Result<(), StorageError> {
        fs::create_dir_all(&self.history_audio_dir)?;
        self.retry_pending_audio_deletions()?;
        let referenced = {
            let connection = self.connection()?;
            let mut statement = connection.prepare(
                "SELECT id, audio_filename FROM dictation_history WHERE audio_filename IS NOT NULL",
            )?;
            let values = statement
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            values
        };
        for (id, filename) in &referenced {
            let exists = self.audio_path(filename).is_ok_and(|path| path.is_file());
            if !exists {
                self.connection()?.execute(
                    "UPDATE dictation_history SET audio_filename = NULL WHERE id = ?1",
                    [id],
                )?;
            }
        }
        let live_filenames = {
            let connection = self.connection()?;
            let mut statement = connection.prepare(
                "SELECT audio_filename FROM dictation_history WHERE audio_filename IS NOT NULL",
            )?;
            let values = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            values
        };
        for entry in fs::read_dir(&self.history_audio_dir)? {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let filename = entry.file_name().to_string_lossy().into_owned();
            if !live_filenames.iter().any(|value| value == &filename) {
                // `read_dir` produced this contained path. Delete that path directly
                // so a malformed orphan cannot turn startup into a path-validation
                // failure or escape the owned directory.
                if fs::remove_file(entry.path()).is_err() && self.audio_path(&filename).is_ok() {
                    let _ = self.connection()?.execute(
                        "INSERT OR IGNORE INTO pending_audio_deletions(filename, created_at) VALUES (?1, ?2)",
                        params![filename, Utc::now().to_rfc3339()],
                    );
                }
            } else if filename.ends_with(".stage") {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    fn delete_history_matching(&self, days: Option<i64>) -> Result<(), StorageError> {
        let cutoff = days.map(|days| (Utc::now() - chrono::Duration::days(days)).to_rfc3339());
        self.delete_history_before(cutoff.as_deref())
    }

    fn delete_history_before(&self, cutoff: Option<&str>) -> Result<(), StorageError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let removed = {
            let mut statement = transaction.prepare(
                "SELECT id, audio_filename FROM dictation_history
                 WHERE ?1 IS NULL OR created_at < ?1",
            )?;
            let values = statement
                .query_map(params![cutoff], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            values
        };
        for (_, filename) in &removed {
            if let Some(filename) = filename {
                transaction.execute(
                    "INSERT OR IGNORE INTO pending_audio_deletions(filename, created_at) VALUES (?1, ?2)",
                    params![filename, Utc::now().to_rfc3339()],
                )?;
            }
        }
        for (id, _) in &removed {
            transaction.execute("DELETE FROM dictation_history WHERE id = ?1", [id])?;
        }
        purge_dictionary_candidates_before(&transaction, cutoff)?;
        transaction.commit()?;
        drop(connection);
        let _ = self.retry_pending_audio_deletions();
        Ok(())
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
            // A blank priority cell means the default priority, as in the form.
            let priority = match row[4].trim() {
                "" => 0,
                value => value.parse::<i64>().map_err(|_| {
                    StorageError::Validation(format!(
                        "CSV row {} has an invalid priority",
                        index + 2
                    ))
                })?,
            };
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
        let history_off = stored_settings
            .as_deref()
            .map(stored_history_is_off)
            .transpose()?
            .unwrap_or(false);
        if history_off {
            return Ok(None);
        }
        let already_seen: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM dictionary_candidates WHERE original_span = ?1 AND preferred_span = ?2
              UNION SELECT 1 FROM dictionary_candidate_decisions WHERE original_span = ?1 AND preferred_span = ?2)",
            params![original_span, preferred_span], |row| row.get(0),
        )?;
        // Any entry, in any scope, that already spells the preferred span as
        // its surface or an alias represents it. Proposing it again would
        // either duplicate that entry globally or fail on an alias collision
        // at every confirmation.
        if already_seen
            || represented_entry(&read_dictionary_entries(&connection)?, &preferred_span).is_some()
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
        let settings = self.get_settings()?;
        if settings.history_retention == HistoryRetention::Never {
            return Ok(Vec::new());
        }
        // Like History, expired candidate text is purged before it is read.
        self.apply_history_policy(&settings)?;
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
        // A candidate proposed before a matching entry existed resolves to
        // that entry unchanged instead of creating a duplicate or failing.
        let existing = read_dictionary_entries(&transaction)?;
        let match_id = represented_entry(&existing, &surface).map(|item| item.id);
        let result_id = match match_id {
            Some(id) => id,
            None => insert_dictionary_entry(&transaction, &entry, "auto", None)?,
        };
        transaction.execute("INSERT OR IGNORE INTO dictionary_candidate_decisions(original_span, preferred_span, created_at) VALUES (?1, ?2, ?3)", params![reading, surface, Utc::now().to_rfc3339()])?;
        transaction.execute(
            "DELETE FROM dictionary_candidates WHERE original_span = ?1 AND preferred_span = ?2",
            params![reading, surface],
        )?;
        let result = read_dictionary_entries(&transaction)?
            .into_iter()
            .find(|item| item.id == result_id);
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

    pub fn dictionary_asr_prompt_for(
        &self,
        context: Option<&crate::types::AppContext>,
        backend: &str,
    ) -> Result<Option<String>, StorageError> {
        let entries = self
            .list_dictionary()?
            .into_iter()
            .filter(|entry| scope_matches(entry.app_scope.as_deref(), context))
            .collect::<Vec<_>>();
        let mut terms = Vec::new();
        let mut length = 0;
        let mut push_within = |term: String, separator: usize, budget: usize| {
            let separator = if terms.is_empty() { 0 } else { separator };
            let size = term.chars().count() + separator;
            if length + size <= budget {
                length += size;
                terms.push(term);
            }
        };
        if backend == "faster-whisper" {
            // Hotwords are plain terms, not the `reading => surface` syntax
            // used by prompt-based backends. Every surface, in priority
            // order, comes before any alias, and aliases before readings, so
            // a high-priority entry's variants cannot spend the small budget
            // ahead of lower-priority surfaces. Readings of auto entries are
            // the misrecognized form and are never hotwords.
            let mut seen = std::collections::HashSet::new();
            for term in entries
                .iter()
                .map(|entry| entry.surface.as_str())
                .chain(
                    entries
                        .iter()
                        .flat_map(|entry| entry.aliases.iter().map(String::as_str)),
                )
                .chain(
                    entries
                        .iter()
                        .filter(|entry| entry.source == "manual")
                        .map(|entry| entry.reading.as_str()),
                )
            {
                let term = term.trim();
                if !term.is_empty() && seen.insert(term.to_lowercase()) {
                    push_within(term.to_owned(), 2, 200);
                }
            }
            return Ok((!terms.is_empty()).then(|| terms.join(", ")));
        }
        for entry in entries {
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
            push_within(term, 1, 1200);
        }
        Ok((!terms.is_empty()).then(|| terms.join("\n")))
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

/// The entry that already spells `span` as its surface or an alias,
/// case-insensitively and in any scope. A global surface match is preferred,
/// then any surface, then any alias.
fn represented_entry<'a>(
    entries: &'a [DictionaryEntry],
    span: &str,
) -> Option<&'a DictionaryEntry> {
    let span = span.trim().to_lowercase();
    let surface_matches = |entry: &&DictionaryEntry| entry.surface.trim().to_lowercase() == span;
    entries
        .iter()
        .filter(surface_matches)
        .find(|entry| normalized_scope(entry.app_scope.as_deref()).is_none())
        .or_else(|| entries.iter().find(surface_matches))
        .or_else(|| {
            entries.iter().find(|entry| {
                entry
                    .aliases
                    .iter()
                    .any(|alias| alias.trim().to_lowercase() == span)
            })
        })
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

/// Reads the History switch from raw stored settings while the caller holds
/// the connection. Legacy settings without `historyRetention` use
/// `historyEnabled`, as `get_settings` migrates them.
fn stored_history_is_off(raw: &str) -> Result<bool, StorageError> {
    let value: serde_json::Value = serde_json::from_str(raw)?;
    Ok(match value.get("historyRetention") {
        Some(retention) => {
            serde_json::from_value::<HistoryRetention>(retention.clone())?
                == HistoryRetention::Never
        }
        None => {
            value
                .get("historyEnabled")
                .and_then(serde_json::Value::as_bool)
                == Some(false)
        }
    })
}

/// Candidate spans come from transcripts, so they follow the History
/// retention; `None` removes every pending candidate and decision.
fn purge_dictionary_candidates_before(
    connection: &Connection,
    cutoff: Option<&str>,
) -> Result<(), StorageError> {
    connection.execute(
        "DELETE FROM dictionary_candidates WHERE ?1 IS NULL OR created_at < ?1",
        params![cutoff],
    )?;
    connection.execute(
        "DELETE FROM dictionary_candidate_decisions
         WHERE ?1 IS NULL OR created_at IS NULL OR created_at < ?1",
        params![cutoff],
    )?;
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
    // spacing changes. Expand both sides to complete adjacent ASCII terms only:
    // Japanese text has no spaces, so expanding over every alphanumeric
    // character would absorb the surrounding kana/kanji clause.
    let is_term =
        |character: char| character.is_ascii_alphanumeric() || matches!(character, '-' | '_');
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
        .collect::<String>()
        .trim()
        .to_owned();
    let preferred_span = corrected_chars[preferred_start..corrected_end]
        .iter()
        .collect::<String>()
        .trim()
        .to_owned();
    if original_span == preferred_span {
        return None;
    }
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
            // A changed region that still contains other scripts or
            // punctuation spans more than one ASCII term (for example two
            // terms joined by Japanese text) and is not a preferred spelling.
            || !value
                .chars()
                .all(|character| is_term(character) || character.is_whitespace())
            || value
                .chars()
                .all(|character| character.is_ascii_digit() || matches!(character, ' ' | '-' | '_'))
    };
    if invalid(&original_span)
        || invalid(&preferred_span)
        // A single letter (`i` -> `I`) is grammar, not vocabulary.
        || original_normalized.chars().count() <= 1
        || original_normalized != preferred_normalized
        || is_sentence_start_capitalization(
            &original_span,
            &preferred_span,
            &corrected_chars[..preferred_start],
        )
        // Code and URL exclusion applies to the token around each span, so
        // an unrelated contraction or symbol elsewhere does not disable
        // detection, while `github.com` or `foo()` still does.
        || has_code_or_url(&span_token(&original_chars, original_start, original_end))
        || has_code_or_url(&span_token(&corrected_chars, preferred_start, corrected_end))
    {
        return None;
    }
    Some((original_span, preferred_span))
}

/// The whitespace-delimited ASCII token containing a span, without
/// surrounding sentence punctuation and intra-word apostrophes (`GitHub's`).
fn span_token(chars: &[char], start: usize, end: usize) -> String {
    let in_token = |character: char| character.is_ascii() && !character.is_ascii_whitespace();
    let mut start = start;
    let mut end = end;
    while start > 0 && in_token(chars[start - 1]) {
        start -= 1;
    }
    while end < chars.len() && in_token(chars[end]) {
        end += 1;
    }
    let token = chars[start..end]
        .iter()
        .collect::<String>()
        .trim()
        .trim_start_matches(['(', '[', '"', '\''])
        .trim_end_matches(['.', ',', '!', '?', ':', ')', ']', '"', '\''])
        .chars()
        .collect::<Vec<_>>();
    token
        .iter()
        .enumerate()
        .filter(|(index, character)| {
            **character != '\''
                || *index == 0
                || !token[index - 1].is_ascii_alphabetic()
                || !token
                    .get(index + 1)
                    .is_some_and(|next| next.is_ascii_alphabetic())
        })
        .map(|(_, character)| *character)
        .collect()
}

fn has_code_or_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let dotted_identifier = value.as_bytes().windows(3).any(|window| {
        window[1] == b'.' && window[0].is_ascii_alphanumeric() && window[2].is_ascii_alphanumeric()
    });
    lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("www.")
        || dotted_identifier
        || value.contains(['/', '\\'])
        || value.contains([
            '`', '{', '}', ';', '(', ')', '[', ']', '=', '<', '>', '#', '@', '$', '&', '|', '"',
            '\'', '+', '*', '%', '!', '^',
        ])
        || value.contains("::")
        || value.contains("->")
        || value.contains("=>")
}

/// True when the spans differ only in the case of their first letter and the
/// span starts a sentence, which is ordinary capitalization rather than a
/// preferred spelling (`hello` -> `Hello`).
fn is_sentence_start_capitalization(original: &str, preferred: &str, before: &[char]) -> bool {
    let mut original_chars = original.chars();
    let mut preferred_chars = preferred.chars();
    let (Some(original_first), Some(preferred_first)) =
        (original_chars.next(), preferred_chars.next())
    else {
        return false;
    };
    if original_first == preferred_first
        || !original_first.eq_ignore_ascii_case(&preferred_first)
        || original_chars.as_str() != preferred_chars.as_str()
    {
        return false;
    }
    before
        .iter()
        .rev()
        .find(|character| !character.is_whitespace() || matches!(character, '\n' | '\r'))
        .is_none_or(|character| {
            matches!(
                character,
                '\n' | '\r'
                    | '.'
                    | '!'
                    | '?'
                    | ':'
                    | '。'
                    | '！'
                    | '？'
                    | '：'
                    | '「'
                    | '『'
                    | '“'
                    | '"'
                    | '-'
                    | '*'
                    | '•'
                    | '・'
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item<'a>() -> NewHistoryItem<'a> {
        NewHistoryItem {
            transcript_text: "private transcript",
            processed_text: Some("processed transcript"),
            source_text: None,
            instruction_text: None,
            action_kind: None,
            search_site: None,
            mode: "faithful",
            asr_provider: "test",
            llm_provider: None,
            target_language: None,
            app_category: None,
            duration_ms: Some(1000),
            latency_ms: Some(200),
            retry_of_id: None,
            insertion_result: None,
            insertion_detail: None,
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
    fn migrates_target_language_column_idempotently() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE dictation_history (
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
                 );",
            )
            .unwrap();
        let storage = Storage::with_connection(
            connection,
            std::env::temp_dir().join(format!("history-migration-{}", random_audio_stem())),
        );

        storage.migrate().unwrap();
        storage.migrate().unwrap();

        let exists: bool = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM pragma_table_info('dictation_history')
                   WHERE name = 'target_language'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists);
        for column in [
            "source_text",
            "instruction_text",
            "action_kind",
            "search_site",
            "insertion_result",
            "insertion_detail",
        ] {
            let exists: bool = storage
                .connection()
                .unwrap()
                .query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM pragma_table_info('dictation_history')
                       WHERE name = ?1
                     )",
                    [column],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "missing migrated column {column}");
        }
    }

    #[test]
    fn translation_history_round_trips_target_language() {
        let storage = Storage::in_memory().unwrap();
        let mut translation = item();
        translation.mode = "translate";
        translation.target_language = Some("ja");
        storage.add_history(&translation).unwrap();

        let stored = storage
            .list_history(HistoryFilter::All, 1)
            .unwrap()
            .remove(0);
        assert_eq!(stored.mode, "translate");
        assert_eq!(stored.target_language.as_deref(), Some("ja"));
    }

    #[test]
    fn history_round_trips_insertion_outcome_codes() {
        let storage = Storage::in_memory().unwrap();
        storage.add_history(&item()).unwrap();
        let mut copied = item();
        copied.insertion_result = Some("clipboard_only");
        copied.insertion_detail = Some("user_activity");
        storage.add_history(&copied).unwrap();

        let rows = storage.list_history(HistoryFilter::All, 2).unwrap();
        let outcomes: Vec<_> = rows
            .iter()
            .map(|row| {
                (
                    row.insertion_result.as_deref(),
                    row.insertion_detail.as_deref(),
                )
            })
            .collect();
        assert!(outcomes.contains(&(None, None)));
        assert!(outcomes.contains(&(Some("clipboard_only"), Some("user_activity"))));
        let id = rows
            .iter()
            .find(|row| row.insertion_result.is_some())
            .unwrap()
            .id;
        let stored = storage.history_item(id).unwrap().unwrap();
        assert_eq!(stored.insertion_result.as_deref(), Some("clipboard_only"));
        assert_eq!(stored.insertion_detail.as_deref(), Some("user_activity"));
    }

    #[test]
    fn edit_history_round_trips_source_instruction_and_result() {
        let storage = Storage::in_memory().unwrap();
        let mut edit = item();
        edit.transcript_text = "make it concise";
        edit.processed_text = Some("Short result.");
        edit.source_text = Some("A verbose original selection.");
        edit.instruction_text = Some("make it concise");
        edit.mode = "edit";
        edit.llm_provider = Some("local");
        storage.add_history(&edit).unwrap();

        let stored = storage
            .list_history(HistoryFilter::All, 1)
            .unwrap()
            .remove(0);
        assert_eq!(stored.mode, "edit");
        assert_eq!(stored.transcript_text, "make it concise");
        assert_eq!(stored.source_text.as_deref(), edit.source_text);
        assert_eq!(stored.instruction_text.as_deref(), edit.instruction_text);
        assert_eq!(stored.processed_text.as_deref(), edit.processed_text);
        assert_eq!(stored.llm_provider.as_deref(), Some("local"));
    }

    #[test]
    fn ask_history_round_trips_action_and_search_site() {
        let storage = Storage::in_memory().unwrap();
        let mut ask = item();
        ask.mode = "ask";
        ask.source_text = Some("selected source");
        ask.instruction_text = Some("search GitHub for rust");
        ask.processed_text = Some("rust");
        ask.action_kind = Some("search");
        ask.search_site = Some("github");
        storage.add_history(&ask).unwrap();
        let stored = storage
            .list_history(HistoryFilter::All, 1)
            .unwrap()
            .remove(0);
        assert_eq!(stored.action_kind.as_deref(), Some("search"));
        assert_eq!(stored.search_site.as_deref(), Some("github"));
        assert_eq!(stored.source_text.as_deref(), ask.source_text);
    }

    #[test]
    fn settings_round_trip() {
        let storage = Storage::in_memory().unwrap();
        let mut settings = Settings::default();
        settings.shortcuts.dictate = vec!["Ctrl+Alt+V".into()];
        settings.history_retention = HistoryRetention::OneWeek;
        settings.local_correction_base_url = "http://127.0.0.1:1234/v1".into();
        settings.local_correction_model = "local-model".into();
        settings.custom_models.push(crate::types::CustomModel {
            asr_backend: "faster-whisper".into(),
            model_id: "community/whisper-custom".into(),
        });
        storage.update_settings(&settings).unwrap();
        assert_eq!(storage.get_settings().unwrap(), settings);
    }

    #[test]
    fn scalar_shortcuts_migrate_once_without_losing_unrelated_json() {
        let storage = Storage::in_memory().unwrap();
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("shortcuts");
        object.insert("hotkey".into(), "Ctrl+Alt+V".into());
        object.insert("voiceTranslateHotkey".into(), "Ctrl+Alt+Y".into());
        object.insert("askHotkey".into(), "Ctrl+Alt+A".into());
        object.insert("speakToEditHotkey".into(), "Ctrl+Alt+E".into());
        object.insert("futureSetting".into(), serde_json::json!({"keep": true}));
        storage
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO settings(key, value, updated_at) VALUES('app_settings', ?1, 'now')",
                [value.to_string()],
            )
            .unwrap();
        let settings = storage.get_settings().unwrap();
        assert_eq!(settings.shortcuts.dictate, ["Ctrl+Alt+V"]);
        assert_eq!(settings.shortcuts.translate, ["Ctrl+Alt+Y"]);
        assert_eq!(settings.shortcuts.ask, ["Ctrl+Alt+A"]);
        assert_eq!(settings.shortcuts.edit, ["Ctrl+Alt+E"]);
        assert_eq!(storage.get_settings().unwrap(), settings);
        let raw: String = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key='app_settings'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let migrated: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(migrated["futureSetting"]["keep"], true);
        assert_eq!(migrated["hotkey"], "Ctrl+Alt+V");
        assert_eq!(migrated["voiceTranslateHotkey"], "Ctrl+Alt+Y");
        assert_eq!(migrated["askHotkey"], "Ctrl+Alt+A");
        assert_eq!(migrated["speakToEditHotkey"], "Ctrl+Alt+E");
        assert_eq!(migrated["shortcuts"]["dictate"][0], "Ctrl+Alt+V");
    }

    #[test]
    fn colliding_legacy_shortcuts_migrate_and_remain_editable() {
        let storage = Storage::in_memory().unwrap();
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("shortcuts");
        object.insert("hotkey".into(), "Ctrl+Shift+T".into());
        let raw = value.to_string();
        storage
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO settings(key, value, updated_at) VALUES('app_settings', ?1, 'now')",
                [raw.as_str()],
            )
            .unwrap();
        let settings = storage.get_settings().unwrap();
        assert_eq!(settings.shortcuts.dictate, ["Ctrl+Shift+T"]);
        let (routes, shadowed) =
            crate::shortcuts::Routes::parse_saved_with_shadowed(&settings).unwrap();
        assert_eq!(routes.0.len(), 4);
        assert_eq!(
            routes
                .find(crate::parse_shortcut("Ctrl+Shift+T").unwrap())
                .unwrap()
                .action,
            crate::shortcuts::Action::Dictate
        );
        assert_eq!(
            shadowed[0].describe(),
            "Selected-text translation (Ctrl+Shift+T)"
        );
        let saved: String = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key='app_settings'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let saved: serde_json::Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved["hotkey"], "Ctrl+Shift+T");
        assert_eq!(saved["shortcuts"]["dictate"][0], "Ctrl+Shift+T");
    }

    #[test]
    fn legacy_selected_text_chord_equal_to_voice_default_stays_selected_text() {
        // A pre-voice-Translate build saved Ctrl+Shift+Y for selected-text
        // translation; migration gives voice Translate the same default.
        let storage = Storage::in_memory().unwrap();
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("shortcuts");
        object.insert("hotkey".into(), "Ctrl+Shift+Space".into());
        object.insert("translationHotkey".into(), "Ctrl+Shift+Y".into());
        storage
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO settings(key, value, updated_at) VALUES('app_settings', ?1, 'now')",
                [value.to_string()],
            )
            .unwrap();
        let settings = storage.get_settings().unwrap();
        assert_eq!(settings.shortcuts.translate, ["Ctrl+Shift+Y"]);
        let (routes, shadowed) =
            crate::shortcuts::Routes::parse_saved_with_shadowed(&settings).unwrap();
        assert_eq!(
            routes
                .find(crate::parse_shortcut("Ctrl+Shift+Y").unwrap())
                .unwrap()
                .action,
            crate::shortcuts::Action::SelectedText
        );
        assert_eq!(shadowed.len(), 1);
        assert_eq!(shadowed[0].describe(), "Voice Translate (Ctrl+Shift+Y)");
    }

    #[test]
    fn saved_settings_mirror_each_primary_chord_for_older_builds() {
        let storage = Storage::in_memory().unwrap();
        let mut settings = Settings::default();
        settings.shortcuts.dictate = vec!["Ctrl+Alt+V".into(), "Ctrl+Shift+Space".into()];
        settings.shortcuts.translate = vec!["Ctrl+Alt+Y".into(), "Ctrl+Alt+U".into()];
        settings.shortcuts.ask = vec!["Ctrl+Alt+A".into()];
        settings.shortcuts.edit = vec!["Ctrl+Alt+E".into(), "Ctrl+Alt+R".into()];
        let read_raw = |storage: &Storage| -> serde_json::Value {
            let raw: String = storage
                .connection()
                .unwrap()
                .query_row(
                    "SELECT value FROM settings WHERE key='app_settings'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            serde_json::from_str(&raw).unwrap()
        };
        for persist in [
            Storage::update_settings as fn(&Storage, &Settings) -> Result<(), StorageError>,
            Storage::update_settings_and_apply_history_policy,
        ] {
            persist(&storage, &settings).unwrap();
            let value = read_raw(&storage);
            assert_eq!(value["hotkey"], "Ctrl+Alt+V");
            assert_eq!(value["voiceTranslateHotkey"], "Ctrl+Alt+Y");
            assert_eq!(value["askHotkey"], "Ctrl+Alt+A");
            assert_eq!(value["speakToEditHotkey"], "Ctrl+Alt+E");
            // Reading keeps the canonical arrays and their mirrors intact.
            assert_eq!(storage.get_settings().unwrap(), settings);
            assert_eq!(read_raw(&storage), value);
        }

        // An older build rewrites settings with its scalar fields only.
        let mut downgraded = read_raw(&storage);
        downgraded.as_object_mut().unwrap().remove("shortcuts");
        storage
            .connection()
            .unwrap()
            .execute(
                "UPDATE settings SET value = ?1 WHERE key = 'app_settings'",
                [downgraded.to_string()],
            )
            .unwrap();
        let upgraded = storage.get_settings().unwrap();
        assert_eq!(upgraded.shortcuts.dictate, ["Ctrl+Alt+V"]);
        assert_eq!(upgraded.shortcuts.translate, ["Ctrl+Alt+Y"]);
        assert_eq!(upgraded.shortcuts.ask, ["Ctrl+Alt+A"]);
        assert_eq!(upgraded.shortcuts.edit, ["Ctrl+Alt+E"]);
    }

    #[test]
    fn empty_persisted_dictation_shortcuts_recover_before_primary_lookup() {
        let storage = Storage::in_memory().unwrap();
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        value["shortcuts"]["dictate"] = serde_json::json!([]);
        value["futureSetting"] = serde_json::json!({"keep": true});
        storage
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO settings(key, value, updated_at) VALUES('app_settings', ?1, 'now')",
                [value.to_string()],
            )
            .unwrap();
        let recovered = storage.get_settings().unwrap();
        assert_eq!(recovered.shortcuts.dictate, ["Ctrl+Shift+Space"]);
        let raw: String = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key='app_settings'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let saved: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(saved["hotkey"], "Ctrl+Shift+Space");
        assert_eq!(saved["shortcuts"]["dictate"][0], "Ctrl+Shift+Space");
        assert_eq!(saved["futureSetting"]["keep"], true);
    }

    #[test]
    fn empty_shortcut_recovery_preserves_unknown_nested_chords() {
        let storage = Storage::in_memory().unwrap();
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        value["shortcuts"]["dictate"] = serde_json::json!([]);
        value["shortcuts"]["ask"] = serde_json::json!([]);
        value["shortcuts"]["futureMode"] = serde_json::json!(["Ctrl+Alt+F"]);
        storage
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO settings(key, value, updated_at) VALUES('app_settings', ?1, 'now')",
                [value.to_string()],
            )
            .unwrap();

        let recovered = storage.get_settings().unwrap();
        assert_eq!(recovered.shortcuts.dictate, ["Ctrl+Shift+Space"]);
        assert_eq!(recovered.shortcuts.ask, ["Ctrl+Shift+A"]);
        let raw: String = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key='app_settings'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let saved: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            saved["shortcuts"]["futureMode"],
            serde_json::json!(["Ctrl+Alt+F"])
        );
        assert_eq!(
            saved["shortcuts"]["dictate"],
            serde_json::json!(["Ctrl+Shift+Space"])
        );
        assert_eq!(
            saved["shortcuts"]["ask"],
            serde_json::json!(["Ctrl+Shift+A"])
        );
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
            correction_mode: "intent_aware".into(),
            ..Settings::default()
        };

        storage.update_settings(&settings).unwrap();

        let restored = storage.get_settings().unwrap();
        assert!(!restored.text_correction_enabled);
        assert_eq!(restored.correction_provider, "gemini");
        assert_eq!(restored.gemini_correction_model, "gemini-custom");
        assert!(!restored.correction_remove_fillers);
        assert!(!restored.correction_auto_format);
        assert_eq!(restored.correction_mode, "intent_aware");
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
    fn legacy_settings_receive_conservative_correction_mode() {
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        value.as_object_mut().unwrap().remove("correctionMode");

        let settings: Settings = serde_json::from_value(value).unwrap();

        assert_eq!(settings.correction_mode, "conservative");
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
        settings.history_retention = HistoryRetention::Never;
        storage.update_settings(&settings).unwrap();

        let mut private_edit = item();
        private_edit.source_text = Some("private selected text");
        private_edit.instruction_text = Some("private spoken instruction");
        assert!(!storage.add_history(&private_edit).unwrap());
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
        let id = storage.list_history(HistoryFilter::All, 10).unwrap()[0].id;
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
            storage.list_history(HistoryFilter::All, 10).unwrap()[0]
                .app_category
                .as_deref(),
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

    /// Lines of the production ASR prompt for a prompt-based backend.
    fn prompt_lines(storage: &Storage, context: Option<&crate::types::AppContext>) -> Vec<String> {
        storage
            .dictionary_asr_prompt_for(context, "openai-compatible")
            .unwrap()
            .map(|prompt| prompt.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    #[test]
    fn asr_prompt_lines_include_reading_surface_and_aliases() {
        let storage = Storage::in_memory().unwrap();
        let aliases = vec!["ChatGPT".into()];
        storage
            .add_dictionary_entry(&dictionary_entry(&aliases))
            .unwrap();
        let terms = prompt_lines(&storage, None);
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
        let terms = prompt_lines(&storage, Some(&context));
        assert_eq!(terms.len(), 3);
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
        // Hotwords route by the same scope rules.
        let hotwords = storage
            .dictionary_asr_prompt_for(Some(&context), "faster-whisper")
            .unwrap()
            .unwrap();
        let hotwords = hotwords.split(", ").collect::<Vec<_>>();
        for surface in ["Global", "App", "Category"] {
            assert!(hotwords.contains(&surface), "missing {surface}");
        }
        assert!(!hotwords.contains(&"Other") && !hotwords.contains(&"other-alias"));
        // Without a context only global entries apply.
        assert_eq!(
            prompt_lines(&storage, None),
            vec!["global-alias => Global (aliases: global-alias)"]
        );
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
            prompt_lines(&storage, failed.as_ref()),
            vec!["global-reading => Global".to_string()]
        );
        assert_eq!(
            storage
                .dictionary_asr_prompt_for(failed.as_ref(), "faster-whisper")
                .unwrap()
                .as_deref(),
            Some("Global, global-reading")
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
        assert_eq!(
            storage.list_history(HistoryFilter::All, 10).unwrap()[0].app_category,
            None
        );

        let unclassified = crate::app_context::from_executable_path(Some(r"C:\Tools\MyEditor.exe"));
        let terms = prompt_lines(&storage, unclassified.as_ref());
        assert_eq!(terms.len(), 3);
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
            .list_history(HistoryFilter::All, 10)
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
        settings.history_retention = HistoryRetention::Never;
        storage.apply_history_policy(&settings).unwrap();
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
    fn settings_write_and_history_off_retention_commit_together() {
        let storage = Storage::in_memory().unwrap();
        storage.add_history(&item()).unwrap();
        let settings = Settings {
            history_retention: HistoryRetention::Never,
            ..Settings::default()
        };
        storage
            .update_settings_and_apply_history_policy(&settings)
            .unwrap();
        assert_eq!(
            storage.get_settings().unwrap().history_retention,
            HistoryRetention::Never
        );
        assert!(storage
            .list_history(HistoryFilter::All, 10)
            .unwrap()
            .is_empty());
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
        storage.apply_history_policy(&settings).unwrap();
        assert_eq!(
            storage.list_history(HistoryFilter::All, 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn retention_uses_strict_utc_cutoff_boundary() {
        let storage = Storage::in_memory().unwrap();
        let cutoff = "2030-01-01T00:00:00+00:00";
        storage
            .connection()
            .unwrap()
            .execute_batch(
                "INSERT INTO dictation_history(transcript_text, mode, asr_provider, created_at)
             VALUES ('exact', 'faithful', 'test', '2030-01-01T00:00:00+00:00');
             INSERT INTO dictation_history(transcript_text, mode, asr_provider, created_at)
             VALUES ('older', 'faithful', 'test', '2029-12-31T23:59:59+00:00');",
            )
            .unwrap();
        storage.delete_history_before(Some(cutoff)).unwrap();
        let texts = storage
            .list_history(HistoryFilter::All, 10)
            .unwrap()
            .into_iter()
            .map(|item| item.transcript_text)
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["exact"]);
    }

    #[test]
    fn all_retention_presets_use_their_documented_windows() {
        for (retention, days) in [
            (HistoryRetention::TwentyFourHours, 1),
            (HistoryRetention::OneWeek, 7),
            (HistoryRetention::OneMonth, 30),
            (HistoryRetention::OneYear, 365),
        ] {
            let storage = Storage::in_memory().unwrap();
            let settings = Settings {
                history_retention: retention,
                ..Settings::default()
            };
            storage.update_settings(&settings).unwrap();
            storage
                .connection()
                .unwrap()
                .execute(
                    "INSERT INTO dictation_history(transcript_text, mode, asr_provider, created_at)
                 VALUES ('old', 'faithful', 'test', ?1)",
                    [(Utc::now() - chrono::Duration::days(days + 1)).to_rfc3339()],
                )
                .unwrap();
            storage.apply_history_policy(&settings).unwrap();
            assert!(storage
                .list_history(HistoryFilter::All, 10)
                .unwrap()
                .is_empty());
            assert_eq!(retention.days(), Some(days));
        }
    }

    fn insert_history_at(storage: &Storage, text: &str, created_at: chrono::DateTime<Utc>) {
        storage
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO dictation_history(transcript_text, mode, asr_provider, created_at)
                 VALUES (?1, 'faithful', 'test', ?2)",
                params![text, created_at.to_rfc3339()],
            )
            .unwrap();
    }

    fn history_texts(storage: &Storage) -> Vec<String> {
        storage
            .list_history(HistoryFilter::All, 10)
            .unwrap()
            .into_iter()
            .map(|item| item.transcript_text)
            .collect()
    }

    fn stored_row_count(storage: &Storage) -> i64 {
        storage
            .connection()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM dictation_history", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn history_audio_files(storage: &Storage) -> Vec<PathBuf> {
        match fs::read_dir(&storage.history_audio_dir) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_file())
                .collect(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("history audio directory is unreadable: {error}"),
        }
    }

    fn source_wav() -> PathBuf {
        let source =
            std::env::temp_dir().join(format!("history-source-{}.wav", random_audio_stem()));
        fs::write(&source, b"RIFF test wav").unwrap();
        source
    }

    #[test]
    fn forever_retention_keeps_rows_of_any_age() {
        let storage = Storage::in_memory().unwrap();
        let settings = Settings {
            history_retention: HistoryRetention::Forever,
            ..Settings::default()
        };
        insert_history_at(
            &storage,
            "ancient",
            Utc::now() - chrono::Duration::days(3650),
        );
        insert_history_at(&storage, "recent", Utc::now());

        storage
            .update_settings_and_apply_history_policy(&settings)
            .unwrap();
        storage.enforce_current_history_policy().unwrap();
        assert!(storage.add_history(&item()).unwrap());

        let texts = history_texts(&storage);
        assert_eq!(texts.len(), 3);
        assert!(texts.iter().any(|text| text == "ancient"));
    }

    #[test]
    fn finite_retention_keeps_rows_inside_the_window_and_purges_older_rows() {
        for retention in [
            HistoryRetention::TwentyFourHours,
            HistoryRetention::OneWeek,
            HistoryRetention::OneMonth,
            HistoryRetention::OneYear,
        ] {
            let storage = Storage::in_memory().unwrap();
            let window = chrono::Duration::days(retention.days().unwrap());
            insert_history_at(
                &storage,
                "inside",
                Utc::now() - window + chrono::Duration::hours(1),
            );
            insert_history_at(
                &storage,
                "outside",
                Utc::now() - window - chrono::Duration::hours(1),
            );

            storage
                .update_settings_and_apply_history_policy(&Settings {
                    history_retention: retention,
                    ..Settings::default()
                })
                .unwrap();

            assert_eq!(history_texts(&storage), vec!["inside"], "{retention:?}");
        }
    }

    #[test]
    fn never_with_source_audio_stores_neither_row_nor_file() {
        let storage = Storage::in_memory().unwrap();
        storage
            .update_settings(&Settings {
                history_retention: HistoryRetention::Never,
                delete_audio_after_processing: false,
                ..Settings::default()
            })
            .unwrap();
        let source = source_wav();

        assert_eq!(
            storage
                .add_history_with_audio_report(&item(), Some(&source))
                .unwrap(),
            (false, false)
        );

        assert_eq!(stored_row_count(&storage), 0);
        assert!(history_audio_files(&storage).is_empty());
        assert!(source.exists(), "the caller still owns its temporary audio");
        let _ = fs::remove_file(source);
        let _ = fs::remove_dir_all(&storage.history_audio_dir);
    }

    #[test]
    fn delete_audio_on_with_source_wav_keeps_text_but_no_history_audio_file() {
        let storage = Storage::in_memory().unwrap();
        storage
            .update_settings(&Settings {
                delete_audio_after_processing: true,
                ..Settings::default()
            })
            .unwrap();
        let source = source_wav();

        assert_eq!(
            storage
                .add_history_with_audio_report(&item(), Some(&source))
                .unwrap(),
            (true, false)
        );

        let row = storage
            .list_history(HistoryFilter::All, 1)
            .unwrap()
            .remove(0);
        assert!(!row.has_audio);
        assert!(storage.history_audio(row.id).unwrap().is_none());
        assert!(history_audio_files(&storage).is_empty());
        let _ = fs::remove_file(source);
        let _ = fs::remove_dir_all(&storage.history_audio_dir);
    }

    #[test]
    fn enabling_delete_audio_keeps_retained_recordings_but_stops_new_retention() {
        let storage = Storage::in_memory().unwrap();
        let mut settings = Settings {
            delete_audio_after_processing: false,
            ..Settings::default()
        };
        storage.update_settings(&settings).unwrap();
        let source = source_wav();
        storage
            .add_history_with_audio(&item(), Some(&source))
            .unwrap();
        let retained = storage
            .list_history(HistoryFilter::All, 1)
            .unwrap()
            .remove(0);
        assert!(retained.has_audio);

        settings.delete_audio_after_processing = true;
        storage
            .update_settings_and_apply_history_policy(&settings)
            .unwrap();
        storage
            .add_history_with_audio(&item(), Some(&source))
            .unwrap();

        // Decision D3: switching the setting on is non-destructive. The old
        // recording stays visible, playable, and deletable; the new row has
        // no audio and no new file is written.
        let rows = storage.list_history(HistoryFilter::All, 10).unwrap();
        assert_eq!(rows.len(), 2);
        let kept = rows.iter().find(|row| row.id == retained.id).unwrap();
        assert!(kept.has_audio);
        assert!(storage.history_audio(retained.id).unwrap().is_some());
        assert!(rows
            .iter()
            .filter(|row| row.id != retained.id)
            .all(|row| !row.has_audio));
        assert_eq!(history_audio_files(&storage).len(), 1);
        assert!(storage.delete_history(retained.id).unwrap());
        assert!(history_audio_files(&storage).is_empty());
        let _ = fs::remove_file(source);
        let _ = fs::remove_dir_all(&storage.history_audio_dir);
    }

    #[test]
    fn failed_audio_stage_preserves_text_only_history() {
        let storage = Storage::in_memory().unwrap();
        storage
            .update_settings(&Settings {
                delete_audio_after_processing: false,
                ..Settings::default()
            })
            .unwrap();
        let unavailable =
            std::env::temp_dir().join(format!("missing-history-{}.wav", random_audio_stem()));
        let (saved, audio_stage_failed) = storage
            .add_history_with_audio_report(&item(), Some(&unavailable))
            .unwrap();
        assert!(saved);
        assert!(audio_stage_failed);
        let row = storage
            .list_history(HistoryFilter::All, 1)
            .unwrap()
            .remove(0);
        assert!(!row.has_audio);
        assert_eq!(row.transcript_text, "private transcript");
    }

    #[test]
    fn deleted_retry_source_keeps_result_without_link() {
        let storage = Storage::in_memory().unwrap();
        storage.add_history(&item()).unwrap();
        let source_id = storage.list_history(HistoryFilter::All, 1).unwrap()[0].id;
        storage.delete_history(source_id).unwrap();
        let mut retry = item();
        retry.retry_of_id = Some(source_id);
        assert!(storage.add_history(&retry).unwrap());
        let row = storage
            .list_history(HistoryFilter::All, 1)
            .unwrap()
            .remove(0);
        assert_eq!(row.retry_of_id, None);
    }

    #[test]
    fn retry_audio_copy_remains_readable_after_source_deletion() {
        let storage = Storage::in_memory().unwrap();
        storage
            .update_settings(&Settings {
                delete_audio_after_processing: false,
                ..Settings::default()
            })
            .unwrap();
        let source =
            std::env::temp_dir().join(format!("history-source-{}.wav", random_audio_stem()));
        fs::write(&source, b"RIFF retry wav").unwrap();
        storage
            .add_history_with_audio(&item(), Some(&source))
            .unwrap();
        let row = storage
            .list_history(HistoryFilter::All, 1)
            .unwrap()
            .remove(0);
        let retry_copy = storage.copy_history_audio_for_retry(row.id).unwrap();
        assert!(storage.delete_history(row.id).unwrap());
        assert_eq!(fs::read(&retry_copy).unwrap(), b"RIFF retry wav");
        let _ = fs::remove_file(retry_copy);
        let _ = fs::remove_file(source);
        let _ = fs::remove_dir_all(&storage.history_audio_dir);
    }

    #[test]
    fn legacy_retention_migration_preserves_unrelated_json_fields() {
        let storage = Storage::in_memory().unwrap();
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("historyRetention");
        object.insert("historyEnabled".into(), serde_json::Value::Bool(true));
        object.insert("historyRetentionDays".into(), serde_json::Value::from(7));
        object.insert("futureSetting".into(), serde_json::Value::from("keep-me"));
        storage
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO settings(key, value, updated_at) VALUES('app_settings', ?1, ?2)",
                params![
                    serde_json::to_string(&value).unwrap(),
                    Utc::now().to_rfc3339()
                ],
            )
            .unwrap();

        assert_eq!(
            storage.get_settings().unwrap().history_retention,
            HistoryRetention::OneWeek
        );
        let persisted: serde_json::Value = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key = 'app_settings'",
                [],
                |row| row.get::<_, String>(0),
            )
            .map(|raw| serde_json::from_str(&raw).unwrap())
            .unwrap();
        assert_eq!(persisted["historyRetention"], "one_week");
        assert_eq!(persisted["futureSetting"], "keep-me");
    }

    #[test]
    fn history_filter_applies_before_limit() {
        let storage = Storage::in_memory().unwrap();
        let mut dictate = item();
        dictate.mode = "faithful";
        storage.add_history(&dictate).unwrap();
        let mut translate = item();
        translate.mode = "translate";
        storage.add_history(&translate).unwrap();
        storage.connection().unwrap().execute(
            "UPDATE dictation_history SET created_at = CASE mode WHEN 'faithful' THEN '2099-01-01T00:00:00Z' ELSE '2099-01-02T00:00:00Z' END", []
        ).unwrap();

        let rows = storage.list_history(HistoryFilter::Dictate, 1).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mode, "faithful");
    }

    #[test]
    fn retained_audio_is_id_only_and_removed_with_its_row() {
        let storage = Storage::in_memory().unwrap();
        let settings = Settings {
            delete_audio_after_processing: false,
            ..Settings::default()
        };
        storage.update_settings(&settings).unwrap();
        let source =
            std::env::temp_dir().join(format!("history-source-{}.wav", random_audio_stem()));
        fs::write(&source, b"RIFF test wav").unwrap();

        assert!(storage
            .add_history_with_audio(&item(), Some(&source))
            .unwrap());
        let row = storage
            .list_history(HistoryFilter::All, 1)
            .unwrap()
            .remove(0);
        let payload = storage.history_audio(row.id).unwrap().unwrap();
        assert_eq!(
            payload.filename,
            format!("local-voice-history-{}.wav", row.id)
        );
        assert_eq!(payload.mime_type, "audio/wav");
        assert_eq!(payload.bytes, b"RIFF test wav");
        let owned_audio: String = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT audio_filename FROM dictation_history WHERE id = ?1",
                [row.id],
                |row| row.get(0),
            )
            .unwrap();
        let owned_path = storage.audio_path(&owned_audio).unwrap();
        assert!(owned_path.exists());
        assert!(storage.delete_history(row.id).unwrap());
        assert!(storage.history_audio(row.id).unwrap().is_none());
        assert!(!owned_path.exists());
        let _ = fs::remove_file(source);
        let _ = fs::remove_dir_all(&storage.history_audio_dir);
    }

    #[test]
    fn delete_all_and_history_off_remove_owned_audio() {
        let storage = Storage::in_memory().unwrap();
        let mut settings = Settings {
            delete_audio_after_processing: false,
            ..Settings::default()
        };
        storage.update_settings(&settings).unwrap();
        let source =
            std::env::temp_dir().join(format!("history-source-{}.wav", random_audio_stem()));
        fs::write(&source, b"RIFF test wav").unwrap();
        storage
            .add_history_with_audio(&item(), Some(&source))
            .unwrap();
        let first = storage
            .list_history(HistoryFilter::All, 1)
            .unwrap()
            .remove(0);
        let first_filename: String = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT audio_filename FROM dictation_history WHERE id = ?1",
                [first.id],
                |row| row.get(0),
            )
            .unwrap();
        let first_path = storage.audio_path(&first_filename).unwrap();
        assert!(first_path.exists());
        assert_eq!(storage.delete_all_history().unwrap(), 1);
        assert!(!first_path.exists());
        storage
            .add_history_with_audio(&item(), Some(&source))
            .unwrap();
        let second = storage
            .list_history(HistoryFilter::All, 1)
            .unwrap()
            .remove(0);
        let second_filename: String = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT audio_filename FROM dictation_history WHERE id = ?1",
                [second.id],
                |row| row.get(0),
            )
            .unwrap();
        let second_path = storage.audio_path(&second_filename).unwrap();
        assert!(second_path.exists());
        settings.history_retention = HistoryRetention::Never;
        storage
            .update_settings_and_apply_history_policy(&settings)
            .unwrap();
        assert!(storage
            .list_history(HistoryFilter::All, 1)
            .unwrap()
            .is_empty());
        assert!(!second_path.exists());
        let _ = fs::remove_file(source);
        let _ = fs::remove_dir_all(&storage.history_audio_dir);
    }

    #[test]
    fn audio_paths_require_generated_relative_wav_names() {
        let storage = Storage::in_memory().unwrap();
        let valid = format!("{}.wav", random_history_audio_stem().unwrap());
        assert!(storage.audio_path(&valid).is_ok());
        for invalid in [
            "..\\outside.wav",
            "C:\\outside.wav",
            "ordinary.wav",
            "abc.txt",
        ] {
            assert!(matches!(
                storage.audio_path(invalid),
                Err(StorageError::InvalidAudioFilename)
            ));
        }
    }

    #[test]
    fn reconciliation_clears_missing_references_and_removes_orphans() {
        let storage = Storage::in_memory().unwrap();
        fs::create_dir_all(&storage.history_audio_dir).unwrap();
        let missing = format!("{}.wav", random_history_audio_stem().unwrap());
        storage.connection().unwrap().execute(
            "INSERT INTO dictation_history(transcript_text, mode, asr_provider, created_at, audio_filename)
             VALUES ('missing', 'faithful', 'test', ?1, ?2)",
            params![Utc::now().to_rfc3339(), missing],
        ).unwrap();
        let orphan = storage.history_audio_dir.join("untrusted-orphan.tmp");
        fs::write(&orphan, b"orphan").unwrap();

        storage.reconcile_history_audio().unwrap();
        let has_audio: bool = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT audio_filename IS NOT NULL FROM dictation_history",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!has_audio);
        assert!(!orphan.exists());
        let _ = fs::remove_dir_all(&storage.history_audio_dir);
    }

    #[test]
    fn retained_audio_filename_is_unique_when_present() {
        let storage = Storage::in_memory().unwrap();
        let filename = format!("{}.wav", random_history_audio_stem().unwrap());
        let now = Utc::now().to_rfc3339();
        storage.connection().unwrap().execute(
            "INSERT INTO dictation_history(transcript_text, mode, asr_provider, created_at, audio_filename)
             VALUES ('first', 'faithful', 'test', ?1, ?2)", params![&now, &filename]
        ).unwrap();
        assert!(storage.connection().unwrap().execute(
            "INSERT INTO dictation_history(transcript_text, mode, asr_provider, created_at, audio_filename)
             VALUES ('second', 'faithful', 'test', ?1, ?2)",
            params![Utc::now().to_rfc3339(), &filename]
        ).is_err());
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
        let csv = "reading,surface,category,aliases,priority,app_scope\n\"read, ing\",Surface,group,\"one|two\",3,app:code\nnext,Other,,,0,\nblank,Blank,,,,\n";
        assert_eq!(storage.import_dictionary_csv(csv).unwrap(), 3);
        let imported = storage.list_dictionary().unwrap();
        assert_eq!(imported.len(), 3);
        // A blank priority cell imports with the default priority.
        assert_eq!(
            imported
                .iter()
                .find(|entry| entry.surface == "Blank")
                .map(|entry| entry.priority),
            Some(0)
        );
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
            assert_eq!(storage.list_dictionary().unwrap().len(), 3);
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
        // Only the numeric-only rule rejects this spacing change: the same
        // change between letters is a candidate.
        assert_eq!(
            detect_dictionary_candidate("call 123 456", "call 123-456"),
            None
        );
        assert_eq!(
            detect_dictionary_candidate("call abc def", "call abc-def"),
            Some(("abc def".into(), "abc-def".into()))
        );
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
            history_retention: HistoryRetention::Never,
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
    fn candidate_spans_stay_on_ascii_terms_in_mixed_script_text() {
        for (original, corrected, expected) in [
            (
                "これはgithubです。",
                "これはGitHubです。",
                ("github", "GitHub"),
            ),
            (
                "明日chat gptを使います",
                "明日ChatGPTを使います",
                ("chat gpt", "ChatGPT"),
            ),
            (
                "資料はopen aiのサイトにあります。",
                "資料はOpenAIのサイトにあります。",
                ("open ai", "OpenAI"),
            ),
            (
                "今日はpythonを書く",
                "今日はPythonを書く",
                ("python", "Python"),
            ),
            (
                "I use python daily",
                "I use Python daily",
                ("python", "Python"),
            ),
        ] {
            assert_eq!(
                detect_dictionary_candidate(original, corrected),
                Some((expected.0.to_owned(), expected.1.to_owned())),
                "{original}"
            );
        }
        for (original, corrected) in [
            // Two changed terms joined by Japanese text are not one term.
            ("githubとgitlab", "GitHubとGitLab"),
            // Sentence-start capitalization is not a preferred spelling.
            ("hello world", "Hello world"),
            ("Thanks. hello there", "Thanks. Hello there"),
            ("githubは便利です", "Githubは便利です"),
            ("了解です。github", "了解です。Github"),
            ("first line\nhello there", "first line\nHello there"),
            // Single-letter spans are grammar, not vocabulary.
            ("so i think", "so I think"),
            ("i think", "I think"),
            ("これはaです", "これはAです"),
        ] {
            assert_eq!(
                detect_dictionary_candidate(original, corrected),
                None,
                "{original}"
            );
        }
    }

    #[test]
    fn candidate_code_and_url_exclusion_applies_to_the_span_only() {
        for (original, corrected) in [
            ("I don't use github", "I don't use GitHub"),
            ("Wow! we use github", "Wow! we use GitHub"),
            ("50% of github users", "50% of GitHub users"),
            ("gpt 3.5 and github", "gpt 3.5 and GitHub"),
            ("(see notes) then github", "(see notes) then GitHub"),
            (
                "see https://example.com and github",
                "see https://example.com and GitHub",
            ),
            ("we like github.", "we like GitHub."),
            ("is it github?", "is it GitHub?"),
            ("we love github's API", "we love GitHub's API"),
            ("これはgithub!です", "これはGitHub!です"),
        ] {
            assert_eq!(
                detect_dictionary_candidate(original, corrected),
                Some(("github".into(), "GitHub".into())),
                "{original}"
            );
        }
        for (original, corrected) in [
            ("open github.com now", "open GitHub.com now"),
            ("open https://github.com now", "open https://GitHub.com now"),
            ("mail me@github now", "mail me@GitHub now"),
            ("call github() now", "call GitHub() now"),
            ("set github; now", "set GitHub; now"),
            ("詳細はwww.github.ioへ", "詳細はwww.GitHub.ioへ"),
        ] {
            assert_eq!(
                detect_dictionary_candidate(original, corrected),
                None,
                "{original}"
            );
        }
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
    fn candidates_already_represented_in_any_scope_are_suppressed_and_resolve() {
        let storage = Storage::in_memory().unwrap();
        let open_aliases = vec!["open ai".to_owned()];
        let scoped = storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "おーぷんえーあい",
                surface: "OpenAI",
                category: None,
                aliases: &open_aliases,
                priority: 5,
                app_scope: Some("app:code"),
            })
            .unwrap();
        let chat_aliases = vec!["chatgpt".to_owned()];
        storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "ちゃっとじーぴーてぃー",
                surface: "Chat GPT service",
                category: None,
                aliases: &chat_aliases,
                priority: 0,
                app_scope: None,
            })
            .unwrap();
        // A scoped surface and a global alias both already represent the span.
        for (original, corrected) in [("open ai", "OpenAI"), ("chat gpt", "ChatGPT")] {
            assert_eq!(
                storage
                    .add_dictionary_candidate_from_correction(original, corrected, None)
                    .unwrap(),
                None,
                "{corrected}"
            );
        }
        assert!(storage.list_dictionary_candidates().unwrap().is_empty());

        // Candidates proposed before a matching entry existed resolve to that
        // entry instead of creating a global duplicate or failing on a
        // collision at every confirmation.
        let alias_pending = storage
            .add_dictionary_candidate_from_correction("git hub", "GitHub", None)
            .unwrap()
            .unwrap();
        let surface_pending = storage
            .add_dictionary_candidate_from_correction("type script", "TypeScript", None)
            .unwrap()
            .unwrap();
        let hub_aliases = vec!["github".to_owned()];
        let alias_owner = storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "ぎっとはぶ",
                surface: "GitHub Enterprise",
                category: None,
                aliases: &hub_aliases,
                priority: 0,
                app_scope: None,
            })
            .unwrap();
        let no_aliases = Vec::new();
        let scoped_surface = storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "たいぷすくりぷと",
                surface: "typescript",
                category: None,
                aliases: &no_aliases,
                priority: 0,
                app_scope: Some("category:development"),
            })
            .unwrap();
        let before = storage.list_dictionary().unwrap();
        assert_eq!(
            storage
                .confirm_dictionary_candidate(alias_pending)
                .unwrap()
                .map(|entry| entry.id),
            Some(alias_owner)
        );
        assert_eq!(
            storage
                .confirm_dictionary_candidate(surface_pending)
                .unwrap()
                .map(|entry| entry.id),
            Some(scoped_surface)
        );
        assert_eq!(storage.list_dictionary().unwrap(), before);
        assert!(storage.list_dictionary_candidates().unwrap().is_empty());
        assert!(before
            .iter()
            .any(|entry| entry.id == scoped && entry.source == "manual"));
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
    fn asr_prompt_orders_by_priority_truncates_to_budget_and_omits_auto_readings() {
        let storage = Storage::in_memory().unwrap();
        let no_aliases = Vec::new();
        let long_reading = "x".repeat(100);
        // Inserted out of order; each manual line is 110 characters, so ten
        // lines plus separators fit the 1200-character prompt budget.
        for index in [3, 12, 1, 7, 10, 2, 5, 11, 4, 9, 6, 8] {
            let surface = format!("Term{index:02}");
            storage
                .add_dictionary_entry(&NewDictionaryEntry {
                    reading: &long_reading,
                    surface: &surface,
                    category: None,
                    aliases: &no_aliases,
                    priority: index,
                    app_scope: None,
                })
                .unwrap();
        }
        let prompt = storage
            .dictionary_asr_prompt_for(None, "openai-compatible")
            .unwrap()
            .unwrap();
        assert!(prompt.chars().count() <= 1200);
        let expected = (3..=12)
            .rev()
            .map(|index| format!("{long_reading} => Term{index:02}"))
            .collect::<Vec<_>>();
        assert_eq!(prompt.lines().collect::<Vec<_>>(), expected);
        let hotwords = storage
            .dictionary_asr_prompt_for(None, "faster-whisper")
            .unwrap()
            .unwrap();
        // Every surface precedes the shared reading, which appears once.
        let surfaces = (1..=12)
            .rev()
            .map(|index| format!("Term{index:02}"))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(hotwords, format!("{surfaces}, {long_reading}"));
        assert!(hotwords.chars().count() <= 200);

        // A confirmed candidate's reading is the misrecognized form: prompts
        // carry only its surface and hotwords never include it.
        let storage = Storage::in_memory().unwrap();
        let candidate = storage
            .add_dictionary_candidate_from_correction("auto term", "AutoTerm", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            storage
                .confirm_dictionary_candidate(candidate)
                .unwrap()
                .unwrap()
                .reading,
            "auto term"
        );
        assert_eq!(prompt_lines(&storage, None), vec!["AutoTerm"]);
        assert_eq!(
            storage
                .dictionary_asr_prompt_for(None, "faster-whisper")
                .unwrap()
                .as_deref(),
            Some("AutoTerm")
        );
    }

    #[test]
    fn hotwords_list_surfaces_then_aliases_then_readings_without_duplicates() {
        let storage = Storage::in_memory().unwrap();
        let high_aliases = vec!["High Alias".to_owned()];
        let no_aliases = Vec::new();
        for (reading, surface, aliases, priority) in [
            ("high reading", "High", &high_aliases, 9),
            // Readings equal to an earlier term are not repeated.
            ("high alias", "Mid", &no_aliases, 5),
            ("low", "Low", &no_aliases, 1),
        ] {
            storage
                .add_dictionary_entry(&NewDictionaryEntry {
                    reading,
                    surface,
                    category: None,
                    aliases,
                    priority,
                    app_scope: None,
                })
                .unwrap();
        }
        let candidate = storage
            .add_dictionary_candidate_from_correction("auto term", "AutoTerm", None)
            .unwrap()
            .unwrap();
        storage.confirm_dictionary_candidate(candidate).unwrap();
        assert_eq!(
            storage
                .dictionary_asr_prompt_for(None, "faster-whisper")
                .unwrap()
                .as_deref(),
            Some("High, Mid, Low, AutoTerm, High Alias, high reading")
        );

        // A long reading of a high-priority entry no longer consumes the
        // budget ahead of a lower-priority surface.
        let storage = Storage::in_memory().unwrap();
        let long_reading = "x".repeat(190);
        for (reading, surface, priority) in [(long_reading.as_str(), "Alpha", 9), ("b", "Beta", 1)]
        {
            storage
                .add_dictionary_entry(&NewDictionaryEntry {
                    reading,
                    surface,
                    category: None,
                    aliases: &no_aliases,
                    priority,
                    app_scope: None,
                })
                .unwrap();
        }
        assert_eq!(
            storage
                .dictionary_asr_prompt_for(None, "faster-whisper")
                .unwrap()
                .as_deref(),
            Some("Alpha, Beta, b")
        );
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
        storage.apply_history_policy(&settings).unwrap();
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
        settings.history_retention = HistoryRetention::Never;
        storage
            .update_settings_and_apply_history_policy(&settings)
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

    #[test]
    fn candidate_text_follows_history_retention_and_legacy_history_switch() {
        assert!(stored_history_is_off(r#"{"historyRetention":"never"}"#).unwrap());
        assert!(!stored_history_is_off(r#"{"historyRetention":"forever"}"#).unwrap());
        assert!(stored_history_is_off(r#"{"historyEnabled":false}"#).unwrap());
        assert!(!stored_history_is_off(r#"{"historyEnabled":true}"#).unwrap());
        assert!(!stored_history_is_off("{}").unwrap());

        let storage = Storage::in_memory().unwrap();
        let settings = Settings {
            history_retention: HistoryRetention::Forever,
            ..Settings::default()
        };
        storage
            .update_settings_and_apply_history_policy(&settings)
            .unwrap();
        storage
            .add_dictionary_candidate_from_correction("open ai", "OpenAI", None)
            .unwrap()
            .unwrap();
        let connection = storage.connection().unwrap();
        connection
            .execute(
                "UPDATE dictionary_candidates SET created_at = '2000-01-01T00:00:00Z'",
                [],
            )
            .unwrap();
        drop(connection);
        storage.enforce_current_history_policy().unwrap();
        assert_eq!(storage.list_dictionary_candidates().unwrap().len(), 1);
    }

    #[test]
    fn listing_candidates_purges_text_past_the_retention_window() {
        let storage = Storage::in_memory().unwrap();
        let settings = Settings {
            history_retention: HistoryRetention::OneMonth,
            ..Settings::default()
        };
        storage
            .update_settings_and_apply_history_policy(&settings)
            .unwrap();
        let expired = storage
            .add_dictionary_candidate_from_correction("open ai", "OpenAI", None)
            .unwrap()
            .unwrap();
        let current = storage
            .add_dictionary_candidate_from_correction("use Github", "use GitHub", None)
            .unwrap()
            .unwrap();
        let connection = storage.connection().unwrap();
        connection
            .execute(
                "UPDATE dictionary_candidates SET created_at = '2000-01-01T00:00:00Z' WHERE id = ?1",
                [expired],
            )
            .unwrap();
        drop(connection);
        let listed = storage.list_dictionary_candidates().unwrap();
        assert_eq!(
            listed.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![current]
        );
    }
}
