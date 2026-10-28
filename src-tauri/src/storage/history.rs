//! History queries and transactional deletion. Column order has one owner.
use super::{
    checkpoint_deleted_history, insert_history_row, purge_dictionary_candidates_before,
    purge_history, purge_metrics_before, queue_audio_deletion, stored_settings, Storage,
    StorageError,
};
use crate::types::{HistoryCursor, HistoryFilter, HistoryItem, HistoryPage, HistoryRetention};
use chrono::Utc;
use rusqlite::{params, OptionalExtension, Row};

const HISTORY_SELECT: &str =
    "SELECT id, transcript_text, processed_text, mode, asr_provider, llm_provider,
    target_language, app_category, duration_ms, latency_ms, created_at,
    source_text, instruction_text, action_kind, search_site,
    audio_filename IS NOT NULL, retry_of_id, insertion_result, insertion_detail, expires_at
    FROM dictation_history";

fn row_to_history_item(row: &Row<'_>) -> rusqlite::Result<HistoryItem> {
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
        expires_at: row.get(19)?,
    })
}

fn retained_history_ids(
    connection: &rusqlite::Connection,
    known_ids: &[i64],
) -> Result<Vec<i64>, StorageError> {
    let mut statement = connection.prepare(
        "SELECT id FROM dictation_history WHERE id IN (SELECT value FROM json_each(?1))",
    )?;
    let rows = statement.query_map([serde_json::to_string(known_ids)?], |row| row.get(0))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

impl Storage {
    /// Text-only retries cannot resurrect a source deleted while AI was running.
    pub fn save_history_text_retry(
        &self,
        source_id: i64,
        item: &crate::types::NewHistoryItem<'_>,
    ) -> Result<Option<bool>, StorageError> {
        let mut connection = self.connection()?;
        let settings = stored_settings(&connection)?;
        let transaction = connection.transaction()?;
        let now = Utc::now();
        purge_history(&transaction, &settings, now)?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM dictation_history WHERE id = ?1)",
            [source_id],
            |row| row.get(0),
        )?;
        let saved = exists && settings.history_retention != HistoryRetention::Never;
        if saved {
            insert_history_row(&transaction, item, &now.to_rfc3339(), None, None)?;
        }
        transaction.commit()?;
        checkpoint_deleted_history(&connection);
        drop(connection);
        let _ = self.retry_pending_audio_deletions();
        Ok(exists.then_some(saved))
    }

    #[cfg(test)]
    pub fn list_history(
        &self,
        filter: HistoryFilter,
        limit: u32,
    ) -> Result<Vec<HistoryItem>, StorageError> {
        self.list_history_page(filter, limit, None, "")
    }

    #[cfg(test)]
    pub fn list_history_page(
        &self,
        filter: HistoryFilter,
        limit: u32,
        before: Option<&HistoryCursor>,
        query: &str,
    ) -> Result<Vec<HistoryItem>, StorageError> {
        Ok(self
            .query_history_page(filter, limit, before, query, &[])?
            .items)
    }

    pub fn query_history_page(
        &self,
        filter: HistoryFilter,
        limit: u32,
        before: Option<&HistoryCursor>,
        query: &str,
        known_ids: &[i64],
    ) -> Result<HistoryPage, StorageError> {
        let settings = self.get_settings()?;
        self.apply_history_policy(&settings)?;
        // With History off, only failed takes kept for Retry remain listed.
        let history_off = settings.history_retention == HistoryRetention::Never;
        let connection = self.connection()?;
        let mut statement = connection.prepare(&format!(
            "{HISTORY_SELECT}
             WHERE (NOT ?3 OR expires_at IS NOT NULL) AND (?1 = 'all' OR
               (?1 = 'dictate' AND mode IN ('faithful', 'ai_corrected', 'faithful_fallback')) OR
               (?1 = 'translate' AND mode IN ('translate', 'selected_translate')) OR
               (?1 = 'edit' AND mode = 'edit') OR
               (?1 = 'ask' AND mode = 'ask'))
             AND (?4 = '' OR instr(lower(transcript_text), lower(?4)) > 0
               OR instr(lower(COALESCE(processed_text, '')), lower(?4)) > 0
               OR instr(lower(COALESCE(source_text, '')), lower(?4)) > 0
               OR instr(lower(COALESCE(instruction_text, '')), lower(?4)) > 0)
             AND (?5 IS NULL OR created_at < ?5 OR (created_at = ?5 AND id < ?6))
             ORDER BY created_at DESC, id DESC LIMIT ?2"
        ))?;
        let filter = match filter {
            HistoryFilter::All => "all",
            HistoryFilter::Dictate => "dictate",
            HistoryFilter::Translate => "translate",
            HistoryFilter::Edit => "edit",
            HistoryFilter::Ask => "ask",
        };
        let rows = statement.query_map(
            params![
                filter,
                limit.min(500),
                history_off,
                query.trim().chars().take(500).collect::<String>(),
                before.map(|cursor| cursor.created_at.as_str()),
                before.map(|cursor| cursor.id)
            ],
            row_to_history_item,
        )?;
        let items = rows.collect::<Result<Vec<_>, _>>()?;
        // Keep the page and previously displayed IDs in one locked snapshot;
        // concurrent deletion cannot invalidate half of this response.
        let retained_ids = retained_history_ids(&connection, known_ids)?;
        Ok(HistoryPage {
            items,
            retained_ids,
        })
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
                &format!("{HISTORY_SELECT} WHERE id = ?1"),
                [id],
                row_to_history_item,
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
            queue_audio_deletion(&transaction, filename, &Utc::now().to_rfc3339())?;
        }
        let changed =
            transaction.execute("DELETE FROM dictation_history WHERE id = ?1", [id])? != 0;
        transaction.commit()?;
        checkpoint_deleted_history(&connection);
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
            queue_audio_deletion(&transaction, filename, &Utc::now().to_rfc3339())?;
        }
        let changed = transaction.execute("DELETE FROM dictation_history", [])? as u64;
        // Candidate spans and decisions are transcript text, so removing all
        // History removes them too. Confirmed dictionary entries are kept.
        purge_dictionary_candidates_before(&transaction, None)?;
        purge_metrics_before(&transaction, None)?;
        transaction.commit()?;
        checkpoint_deleted_history(&connection);
        drop(connection);
        let _ = self.retry_pending_audio_deletions();
        Ok(changed)
    }
}
