//! Transactional schema upgrades and reader compatibility.
use super::StorageError;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool, StorageError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        params![table, column],
        |row| row.get(0),
    )?)
}

/// One schema step. It runs inside the transaction that also advances
/// `PRAGMA user_version`, so a failed step leaves the previous version.
pub(super) struct Migration {
    pub(super) apply: fn(&Connection) -> Result<(), StorageError>,
    /// Lowest schema version a build must support to keep using the
    /// database after this step. An additive step (new tables or nullable
    /// columns that older code ignores) keeps 1, so older builds still open
    /// it; only a step that breaks older readers raises it.
    pub(super) min_reader_version: i64,
}

/// Ordered schema steps; step `n` (0-based) upgrades `user_version` `n` to
/// `n + 1`. Append new steps; never edit or reorder released ones.
pub(super) const MIGRATIONS: &[Migration] = &[
    Migration {
        apply: migrate_v1_baseline,
        min_reader_version: 1,
    },
    Migration {
        apply: migrate_v2_settings_backups,
        min_reader_version: 1,
    },
];

/// Key in `schema_meta` holding the highest `min_reader_version` of every
/// applied step.
pub(super) const MIN_READER_VERSION_KEY: &str = "min_reader_version";

/// The compatibility marker a database declares; 1 when it has none.
pub(super) fn stored_min_reader_version(connection: &Connection) -> Result<i64, StorageError> {
    let has_meta: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta')",
        [],
        |row| row.get(0),
    )?;
    if !has_meta {
        return Ok(1);
    }
    Ok(connection
        .query_row(
            "SELECT value FROM schema_meta WHERE key = ?1",
            [MIN_READER_VERSION_KEY],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(1))
}

/// Applies every step above the stored `user_version`, one transaction per
/// step, and records the steps' compatibility marker with it.
///
/// A database from a newer build (a higher `user_version`) is opened as is
/// when its marker says this build can still use it: no unknown step runs
/// and `user_version` is never lowered. It is refused only when the marker
/// exceeds this build's version.
pub(super) fn run_migrations(
    connection: &mut Connection,
    migrations: &[Migration],
) -> Result<(), StorageError> {
    let supported = i64::try_from(migrations.len()).unwrap_or(i64::MAX);
    loop {
        // IMMEDIATE takes the write lock before the version is read, so two
        // processes cannot both apply the same step.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == supported {
            return Ok(());
        }
        let Some(step) = usize::try_from(version)
            .ok()
            .and_then(|index| migrations.get(index))
        else {
            let min_reader_version = stored_min_reader_version(&transaction)?;
            if version < 0 || min_reader_version > supported {
                return Err(StorageError::IncompatibleSchema {
                    version,
                    min_reader_version,
                    supported,
                });
            }
            eprintln!(
                "Database schema version {version} is newer than this build's {supported}; opening it because it remains compatible with version {min_reader_version}."
            );
            return Ok(());
        };
        (step.apply)(&transaction)?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_meta (
               key TEXT PRIMARY KEY NOT NULL,
               value INTEGER NOT NULL
             );",
        )?;
        let min_reader_version =
            stored_min_reader_version(&transaction)?.max(step.min_reader_version);
        transaction.execute(
            "INSERT INTO schema_meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![MIN_READER_VERSION_KEY, min_reader_version],
        )?;
        transaction.pragma_update(None, "user_version", version + 1)?;
        transaction.commit()?;
    }
}

/// Version 1: the schema as it stood before versioning. Databases from
/// earlier builds have `user_version` 0 and any earlier additive shape, so
/// every step probes and is idempotent.
fn migrate_v1_baseline(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
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
    if !column_exists(connection, "dictation_history", "target_language")? {
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
        if !column_exists(connection, "dictation_history", column)? {
            connection.execute(
                &format!("ALTER TABLE dictation_history ADD COLUMN {column} TEXT"),
                [],
            )?;
        }
    }
    for (column, definition) in [
        ("audio_filename", "TEXT"),
        ("expires_at", "TEXT"),
        (
            "retry_of_id",
            "INTEGER REFERENCES dictation_history(id) ON DELETE SET NULL",
        ),
    ] {
        if !column_exists(connection, "dictation_history", column)? {
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
    if !column_exists(connection, "dictionary_entries", "source")? {
        connection.execute(
            "ALTER TABLE dictionary_entries ADD COLUMN source TEXT NOT NULL DEFAULT 'manual'",
            [],
        )?;
    }
    connection.execute("UPDATE dictionary_entries SET source = 'manual' WHERE source IS NULL OR source NOT IN ('manual', 'auto')", [])?;
    if !column_exists(connection, "dictionary_candidate_decisions", "created_at")? {
        // An unreleased development database may have the old table. Its
        // undated private spans cannot be retained under a time policy.
        connection.execute(
            "ALTER TABLE dictionary_candidate_decisions ADD COLUMN created_at TEXT",
            [],
        )?;
    }
    Ok(())
}

/// Version 2: raw settings rows kept before a repair rewrites them, so a
/// value this build could not read is never lost.
fn migrate_v2_settings_backups(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings_backups (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           key TEXT NOT NULL,
           value TEXT NOT NULL,
           reason TEXT NOT NULL,
           created_at TEXT NOT NULL
         );",
    )?;
    Ok(())
}
