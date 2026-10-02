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
    DictionaryEntry, HistoryAudioPayload, HistoryFilter, HistoryItem, HistoryRetention,
    NewDictionaryEntry, NewHistoryItem, Settings,
};

const MAX_HISTORY_AUDIO_BYTES: u64 = 50 * 1024 * 1024;
#[cfg(test)]
static AUDIO_NONCE: AtomicU64 = AtomicU64::new(1);

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
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL
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
               latency_ms, created_at, audio_filename, retry_of_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                      CASE WHEN EXISTS (SELECT 1 FROM dictation_history WHERE id = ?16) THEN ?16 ELSE NULL END)",
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
                    audio_filename IS NOT NULL, retry_of_id
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
                    audio_filename IS NOT NULL, retry_of_id
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
        transaction.commit()?;
        drop(connection);
        let _ = self.retry_pending_audio_deletions();
        Ok(())
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
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT surface, aliases
             FROM dictionary_entries ORDER BY priority DESC, surface ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (surface, aliases) = row?;
            terms.push(surface);
            terms.extend(serde_json::from_str::<Vec<String>>(&aliases)?);
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
}
