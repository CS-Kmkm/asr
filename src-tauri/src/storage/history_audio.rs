//! Audio deletion queue writes, shared by transactional and best-effort cleanup.
use super::StorageError;
use rusqlite::{params, Connection};

pub(super) fn queue_audio_deletion(
    connection: &Connection,
    filename: &str,
    created_at: &str,
) -> Result<(), StorageError> {
    connection.execute(
        "INSERT OR IGNORE INTO pending_audio_deletions(filename, created_at) VALUES (?1, ?2)",
        params![filename, created_at],
    )?;
    Ok(())
}
