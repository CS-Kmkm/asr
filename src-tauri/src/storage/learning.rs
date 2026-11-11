//! Opt-in suggestions from edits the user submits (Issue #85).
//!
//! Nothing here watches typing or keeps text. A suggestion exists only after
//! the user edits a History text and submits it with learning on; only the
//! one changed term pair is stored, never the edited text, and it changes
//! nothing until the user confirms it into a chosen scope.
use super::{
    has_code_or_url, is_sentence_start_capitalization, load_settings, read_dictionary_entries,
    represented_entry, span_token, stored_settings_row, Storage, StorageError,
};
use crate::types::{DictionaryCandidate, DictionaryMode, HistoryRetention, DICTIONARY_MODES};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

pub(super) const USER_EDIT_ORIGIN: &str = "user_edit";
pub(super) const LEARNED_SOURCE: &str = "learned";
/// History rows whose shown text is the user's dictation.
const EDITABLE_HISTORY_MODES: [&str; 3] = ["faithful", "ai_corrected", "faithful_fallback"];
const MAX_EDITED_CHARS: usize = 10_000;
const MAX_SPAN_CHARS: usize = 32;

/// Why an edit produced no suggestion; shown to the user, never the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EditProposalOutcome {
    Proposed,
    NoTermChange,
    AlreadyKnown,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditProposal {
    pub outcome: EditProposalOutcome,
    pub candidate: Option<DictionaryCandidate>,
}

/// The modes column value: NULL for every mode, else a JSON list in the
/// canonical order. Unknown modes are rejected.
pub(super) fn stored_modes(modes: &[String]) -> Result<Option<String>, StorageError> {
    if let Some(unknown) = modes
        .iter()
        .find(|mode| !DICTIONARY_MODES.contains(&mode.as_str()))
    {
        return Err(StorageError::Validation(format!(
            "unknown dictionary mode {unknown}"
        )));
    }
    let selected = DICTIONARY_MODES
        .iter()
        .filter(|mode| modes.iter().any(|value| value == *mode))
        .collect::<Vec<_>>();
    if selected.is_empty() || selected.len() == DICTIONARY_MODES.len() {
        return Ok(None);
    }
    Ok(Some(serde_json::to_string(&selected)?))
}

/// Reads the modes column; an unreadable value applies to every mode.
pub(super) fn read_modes(raw: Option<&str>) -> Vec<String> {
    raw.and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .map(|modes| {
            modes
                .into_iter()
                .filter(|mode| DICTIONARY_MODES.contains(&mode.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn mode_matches(modes: &[String], mode: DictionaryMode) -> bool {
    modes.is_empty() || modes.iter().any(|value| value == mode.as_str())
}

pub(super) fn delete_pending_edit_candidates(
    connection: &Connection,
) -> Result<usize, StorageError> {
    Ok(connection.execute(
        "DELETE FROM dictionary_candidates WHERE origin = ?1",
        [USER_EDIT_ORIGIN],
    )?)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    Latin,
    Japanese,
}

fn is_ascii_term(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
}

fn is_kanji(character: char) -> bool {
    matches!(character, '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}' | '々')
}

fn is_katakana(character: char) -> bool {
    matches!(character, '\u{30A1}'..='\u{30FA}' | 'ー' | '\u{FF66}'..='\u{FF9F}')
}

fn is_hiragana(character: char) -> bool {
    matches!(character, '\u{3041}'..='\u{3096}')
}

/// Characters a changed span may grow over: one ASCII term, or a run of
/// kanji/katakana. Hiragana is not expanded, since it carries particles and
/// inflection (ぎっとはぶ|で).
fn expands(character: char) -> bool {
    is_ascii_term(character) || is_kanji(character) || is_katakana(character)
}

fn script(span: &str) -> Option<Script> {
    let latin = span.chars().any(|c| c.is_ascii_alphabetic());
    let japanese = span
        .chars()
        .any(|c| is_kanji(c) || is_katakana(c) || is_hiragana(c));
    match (latin, japanese) {
        (true, false) => Some(Script::Latin),
        (false, true) => Some(Script::Japanese),
        _ => None,
    }
}

/// Whether `original -> preferred` changes more than one term: two changed
/// regions with an unchanged word boundary (a space or a hiragana particle)
/// between them, as in `Kubernetis and Dockre -> Kubernetes and Docker`.
/// Changes inside one term (`open ai -> OpenAI`) do not count.
fn changes_several_terms(original: &str, preferred: &str) -> bool {
    let left = original.chars().collect::<Vec<_>>();
    let right = preferred.chars().collect::<Vec<_>>();
    // Longest common subsequence, walked to find the common runs.
    let mut table = vec![vec![0usize; right.len() + 1]; left.len() + 1];
    for i in (0..left.len()).rev() {
        for j in (0..right.len()).rev() {
            table[i][j] = if left[i] == right[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut changed = false;
    let mut run = Vec::new();
    while i < left.len() && j < right.len() {
        if left[i] == right[j] {
            run.push(left[i]);
            i += 1;
            j += 1;
            continue;
        }
        // A common run between two changes that holds a word boundary
        // separates independent edits.
        if changed && run.iter().any(|c| *c == ' ' || is_hiragana(*c)) {
            return true;
        }
        run.clear();
        changed = true;
        if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    false
}

fn edit_distance(left: &[char], right: &[char]) -> usize {
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (row, left_char) in left.iter().enumerate() {
        let mut current = vec![row + 1];
        for (column, right_char) in right.iter().enumerate() {
            current.push(
                (previous[column + 1] + 1)
                    .min(current[column] + 1)
                    .min(previous[column] + usize::from(left_char != right_char)),
            );
        }
        previous = current;
    }
    previous[right.len()]
}

/// One preferred-spelling pair from a user's edit of `before`, or `None`.
///
/// The edit must change exactly one term: the shortest differing region,
/// grown over adjacent ASCII-term or kanji/katakana characters. Accepted
/// pairs are a reading written in another script (ぎっとはぶ -> GitHub,
/// クロード -> Claude) or a close respelling in the same script
/// (Kubernetis -> Kubernetes, 機会学習 -> 機械学習, open ai -> OpenAI).
/// Unrelated words (Tuesday -> Wednesday, 東京 -> 大阪), changed numbers,
/// grammar-only hiragana edits, URLs, code and multi-line edits are not
/// vocabulary and produce nothing.
pub(crate) fn detect_edit_candidate(before: &str, after: &str) -> Option<(String, String)> {
    let before_chars = before.trim().chars().collect::<Vec<_>>();
    let after_chars = after.trim().chars().collect::<Vec<_>>();
    if before_chars == after_chars {
        return None;
    }
    let mut prefix = 0;
    while prefix < before_chars.len()
        && prefix < after_chars.len()
        && before_chars[prefix] == after_chars[prefix]
    {
        prefix += 1;
    }
    let (mut before_end, mut after_end) = (before_chars.len(), after_chars.len());
    while before_end > prefix
        && after_end > prefix
        && before_chars[before_end - 1] == after_chars[after_end - 1]
    {
        before_end -= 1;
        after_end -= 1;
    }
    // The shared prefix and suffix are identical, so growing both sides over
    // them by the same rule keeps the spans aligned.
    let mut start = prefix;
    while start > 0 && expands(before_chars[start - 1]) {
        start -= 1;
    }
    while before_end < before_chars.len() && expands(before_chars[before_end]) {
        before_end += 1;
        after_end += 1;
    }
    let original = before_chars[start..before_end].iter().collect::<String>();
    let preferred = after_chars[start..after_end].iter().collect::<String>();
    let (original, preferred) = (original.trim(), preferred.trim());
    let allowed = |span: &str| {
        !span.is_empty()
            && span.chars().count() <= MAX_SPAN_CHARS
            && span.chars().all(|c| {
                is_ascii_term(c) || c == ' ' || is_kanji(c) || is_katakana(c) || is_hiragana(c)
            })
    };
    if !allowed(original) || !allowed(preferred) || preferred.chars().count() < 2 {
        return None;
    }
    if preferred.chars().all(is_hiragana) || changes_several_terms(original, preferred) {
        return None;
    }
    let digits = |span: &str| {
        span.chars()
            .filter(char::is_ascii_digit)
            .collect::<String>()
    };
    if digits(original) != digits(preferred)
        || original.chars().all(|c| c.is_ascii_digit() || c == ' ')
    {
        return None;
    }
    if has_code_or_url(&span_token(
        &before_chars,
        start,
        start + original.chars().count(),
    )) || has_code_or_url(&span_token(
        &after_chars,
        start,
        start + preferred.chars().count(),
    )) || is_sentence_start_capitalization(original, preferred, &after_chars[..start])
    {
        return None;
    }
    let (original_script, preferred_script) = (script(original)?, script(preferred)?);
    if original_script == preferred_script {
        let fold = |span: &str| {
            span.chars()
                .filter(|c| !matches!(c, ' ' | '-' | '_'))
                .flat_map(char::to_lowercase)
                .collect::<Vec<_>>()
        };
        let (left, right) = (fold(original), fold(preferred));
        let longest = left.len().max(right.len());
        if left != right && edit_distance(&left, &right) > (longest / 3).max(1) {
            return None;
        }
    }
    Some((original.to_owned(), preferred.to_owned()))
}

impl Storage {
    /// Proposes a suggestion from the user's edit of a History dictation.
    /// Requires learning on and History saving on; stores only the changed
    /// pair and discards `edited`.
    pub fn propose_dictionary_candidate_from_edit(
        &self,
        history_id: i64,
        edited: &str,
    ) -> Result<EditProposal, StorageError> {
        let settings = self.get_settings()?;
        learning_allowed(&settings)?;
        if edited.chars().count() > MAX_EDITED_CHARS
            || edited.chars().any(|c| c.is_control() && !c.is_whitespace())
        {
            return Err(StorageError::Validation(
                "edited text is too long or contains control characters".into(),
            ));
        }
        self.apply_history_policy(&settings)?;
        let item = self
            .history_item(history_id)?
            .filter(|item| item.expires_at.is_none())
            .ok_or_else(|| StorageError::Validation("history item not found".into()))?;
        if !EDITABLE_HISTORY_MODES.contains(&item.mode.as_str()) {
            return Err(StorageError::Validation(
                "only dictation history can be edited".into(),
            ));
        }
        let shown = item
            .processed_text
            .as_deref()
            .unwrap_or(&item.transcript_text);
        let Some((original, preferred)) = detect_edit_candidate(shown, edited) else {
            return Ok(EditProposal {
                outcome: EditProposalOutcome::NoTermChange,
                candidate: None,
            });
        };
        self.store_edit_proposal(history_id, &original, &preferred)
    }

    /// Stores a detected pair. Settings and the History row are checked
    /// again while the connection is held for the insert, so a concurrent
    /// command that turned learning or History off (and purged) since the
    /// first check wins and nothing is stored.
    fn store_edit_proposal(
        &self,
        history_id: i64,
        original: &str,
        preferred: &str,
    ) -> Result<EditProposal, StorageError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let settings = stored_settings_row(&transaction)?
            .map(|raw| load_settings(&raw).settings)
            .unwrap_or_default();
        learning_allowed(&settings)?;
        let eligible: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM dictation_history WHERE id = ?1 AND expires_at IS NULL)",
            [history_id],
            |row| row.get(0),
        )?;
        if !eligible {
            return Err(StorageError::Validation("history item not found".into()));
        }
        let seen: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM dictionary_candidates WHERE original_span = ?1 AND preferred_span = ?2
              UNION SELECT 1 FROM dictionary_candidate_decisions WHERE original_span = ?1 AND preferred_span = ?2)",
            params![original, preferred],
            |row| row.get(0),
        )?;
        if seen || represented_entry(&read_dictionary_entries(&transaction)?, preferred).is_some() {
            return Ok(EditProposal {
                outcome: EditProposalOutcome::AlreadyKnown,
                candidate: None,
            });
        }
        let now = Utc::now().to_rfc3339();
        transaction.execute(
            "INSERT INTO dictionary_candidates(original_span, preferred_span, confidence, history_id, created_at, updated_at, origin)
             VALUES (?1, ?2, 1.0, ?3, ?4, ?4, ?5)",
            params![original, preferred, history_id, now, USER_EDIT_ORIGIN],
        )?;
        let id = transaction.last_insert_rowid();
        let candidate = transaction
            .query_row(
                "SELECT id, original_span, preferred_span, confidence, history_id, created_at, origin
                 FROM dictionary_candidates WHERE id = ?1",
                [id],
                |row| {
                    Ok(DictionaryCandidate {
                        id: row.get(0)?,
                        original_span: row.get(1)?,
                        preferred_span: row.get(2)?,
                        confidence: row.get(3)?,
                        history_id: row.get(4)?,
                        created_at: row.get(5)?,
                        origin: row.get(6)?,
                    })
                },
            )
            .optional()?;
        transaction.commit()?;
        Ok(EditProposal {
            outcome: EditProposalOutcome::Proposed,
            candidate,
        })
    }

    /// Deletes everything learned from edits: pending suggestions, learned
    /// entries (even if edited since), and every confirm/reject decision made
    /// on an edit suggestion. Manual and auto-added entries stay. The WAL is
    /// truncated afterwards like other privacy deletions. Returns how many
    /// rows were deleted.
    pub fn delete_learned_personalization(&self) -> Result<usize, StorageError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let decisions = transaction.execute(
            "DELETE FROM dictionary_candidate_decisions WHERE origin = ?1",
            [USER_EDIT_ORIGIN],
        )?;
        let entries = transaction.execute(
            "DELETE FROM dictionary_entries WHERE source = ?1",
            [LEARNED_SOURCE],
        )?;
        let candidates = delete_pending_edit_candidates(&transaction)?;
        transaction.commit()?;
        self.checkpoint_after_deletion(&connection);
        Ok(decisions + entries + candidates)
    }
}

fn learning_allowed(settings: &crate::types::Settings) -> Result<(), StorageError> {
    if !settings.personalization_learning {
        return Err(StorageError::Validation(
            "learning from edits is off".into(),
        ));
    }
    if settings.history_retention == HistoryRetention::Never {
        return Err(StorageError::Validation(
            "History is off, so suggestions are not stored".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CandidateScopeInput, NewDictionaryEntry, NewHistoryItem, Settings};

    fn pair(before: &str, after: &str) -> Option<(String, String)> {
        detect_edit_candidate(before, after)
    }

    #[test]
    fn term_respellings_and_readings_become_suggestions() {
        for (before, after, original, preferred) in [
            (
                "変更はぎっとはぶにプッシュ",
                "変更はGitHubにプッシュ",
                "ぎっとはぶ",
                "GitHub",
            ),
            ("クロードに聞く", "Claudeに聞く", "クロード", "Claude"),
            (
                "Kubernetisを使う",
                "Kubernetesを使う",
                "Kubernetis",
                "Kubernetes",
            ),
            ("機会学習の論文", "機械学習の論文", "機会学習", "機械学習"),
            ("use open ai today", "use OpenAI today", "open ai", "OpenAI"),
            (
                "we use github daily",
                "we use GitHub daily",
                "github",
                "GitHub",
            ),
        ] {
            assert_eq!(
                pair(before, after),
                Some((original.into(), preferred.into())),
                "{before} -> {after}"
            );
        }
    }

    #[test]
    fn meaning_changes_numbers_grammar_and_code_produce_nothing() {
        for (before, after) in [
            ("Book it for Tuesday please", "Book it for Wednesday please"),
            ("東京で会いましょう", "大阪で会いましょう"),
            ("会議は3時からです", "会議は4時からです"),
            ("GPT4を使う", "GPT5を使う"),
            ("送ります", "送りません"),
            ("明日行きます", "明日行きますね"),
            (
                "open https://a.example/x now",
                "open https://a.example/y now",
            ),
            ("run foo_bar() now", "run foo_baz() now"),
            ("hello world", "Hello world"),
            ("同じ文です", "同じ文です"),
            ("行1\n行2", "行1\n行3と行4"),
            ("Use the API", "Use the"),
            ("Kubernetis and Dockre", "Kubernetes and Docker"),
            (
                "use Kubernetis and Dockre now",
                "use Kubernetes and Docker now",
            ),
            ("機会学習と深相学習", "機械学習と深層学習"),
        ] {
            assert_eq!(pair(before, after), None, "{before} -> {after}");
        }
    }

    fn save(storage: &Storage, settings: Settings) {
        storage
            .update_settings_and_apply_history_policy(&settings)
            .unwrap();
    }

    fn learning_on() -> Settings {
        Settings {
            personalization_learning: true,
            ..Settings::default()
        }
    }

    fn storage_with_history(learning: bool) -> (Storage, i64) {
        let storage = Storage::in_memory().unwrap();
        save(
            &storage,
            Settings {
                personalization_learning: learning,
                ..Settings::default()
            },
        );
        storage
            .add_history(&NewHistoryItem {
                transcript_text: "変更はぎっとはぶにプッシュ",
                processed_text: Some("変更はぎっとはぶにプッシュします"),
                source_text: None,
                instruction_text: None,
                action_kind: None,
                search_site: None,
                mode: "ai_corrected",
                asr_provider: "mock",
                llm_provider: Some("local"),
                target_language: None,
                app_category: None,
                duration_ms: None,
                latency_ms: None,
                retry_of_id: None,
                insertion_result: None,
                insertion_detail: None,
            })
            .unwrap();
        let id = storage
            .list_history(crate::types::HistoryFilter::All, 1)
            .unwrap()[0]
            .id;
        (storage, id)
    }

    #[test]
    fn edits_propose_only_with_learning_on_and_store_no_text() {
        let (storage, id) = storage_with_history(false);
        assert!(storage
            .propose_dictionary_candidate_from_edit(id, "変更はGitHubにプッシュします")
            .is_err());
        assert!(storage.list_dictionary_candidates().unwrap().is_empty());

        let (storage, id) = storage_with_history(true);
        let proposal = storage
            .propose_dictionary_candidate_from_edit(id, "変更はGitHubにプッシュします")
            .unwrap();
        assert_eq!(proposal.outcome, EditProposalOutcome::Proposed);
        let candidates = storage.list_dictionary_candidates().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].origin, USER_EDIT_ORIGIN);
        assert_eq!(
            (
                candidates[0].original_span.as_str(),
                candidates[0].preferred_span.as_str()
            ),
            ("ぎっとはぶ", "GitHub")
        );
        // Only the pair is stored: no column holds the edited sentence.
        let connection = storage.connection().unwrap();
        let stored: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM dictionary_candidates WHERE original_span LIKE '%プッシュ%' OR preferred_span LIKE '%プッシュ%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, 0);
        drop(connection);
        // The same pair is not proposed twice.
        assert_eq!(
            storage
                .propose_dictionary_candidate_from_edit(id, "変更はGitHubにプッシュします")
                .unwrap()
                .outcome,
            EditProposalOutcome::AlreadyKnown
        );
        assert_eq!(
            storage
                .propose_dictionary_candidate_from_edit(id, "まったく別の文章に書き換えました")
                .unwrap()
                .outcome,
            EditProposalOutcome::NoTermChange
        );
    }

    #[test]
    fn history_off_refuses_edits_and_turning_learning_off_drops_pending_suggestions() {
        let (storage, id) = storage_with_history(true);
        storage
            .propose_dictionary_candidate_from_edit(id, "変更はGitHubにプッシュします")
            .unwrap();
        storage
            .add_dictionary_candidate_from_correction("open ai", "OpenAI", None)
            .unwrap();
        save(&storage, Settings::default());
        let remaining = storage.list_dictionary_candidates().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].origin, "correction");

        let (storage, id) = storage_with_history(true);
        save(
            &storage,
            Settings {
                history_retention: HistoryRetention::Never,
                ..learning_on()
            },
        );
        assert!(storage
            .propose_dictionary_candidate_from_edit(id, "変更はGitHubにプッシュします")
            .is_err());
    }

    #[test]
    fn confirmed_edit_suggestions_become_scoped_learned_entries_that_can_be_deleted() {
        let (storage, id) = storage_with_history(true);
        let candidate = storage
            .propose_dictionary_candidate_from_edit(id, "変更はGitHubにプッシュします")
            .unwrap()
            .candidate
            .unwrap();
        let entry = storage
            .confirm_dictionary_candidate_in(
                candidate.id,
                &CandidateScopeInput {
                    app_scope: Some("category:development".into()),
                    modes: vec!["dictate".into(), "edit".into()],
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(entry.source, LEARNED_SOURCE);
        assert_eq!(entry.app_scope.as_deref(), Some("category:development"));
        assert_eq!(entry.modes, vec!["dictate".to_owned(), "edit".to_owned()]);
        let context = crate::types::AppContext {
            app_key: Some("code".into()),
            category: "development".into(),
        };
        let terms = |mode| {
            storage
                .dictionary_correction_terms("ぎっとはぶ", Some(&context), mode)
                .unwrap()
                .len()
        };
        assert_eq!(terms(DictionaryMode::Dictate), 1);
        assert_eq!(terms(DictionaryMode::Edit), 1);
        assert_eq!(terms(DictionaryMode::Ask), 0);
        assert_eq!(
            storage
                .dictionary_correction_terms("ぎっとはぶ", None, DictionaryMode::Dictate)
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            storage
                .search_dictionary(None, Some("learned"))
                .unwrap()
                .len(),
            1
        );
        // Learned data is deleted as a whole; manual entries stay.
        let aliases = Vec::new();
        storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "くろーど",
                surface: "Claude",
                category: None,
                aliases: &aliases,
                priority: 0,
                app_scope: None,
            })
            .unwrap();
        assert!(storage.delete_learned_personalization().unwrap() >= 1);
        let left = storage.list_dictionary().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].surface, "Claude");
        // After deletion the same edit may be suggested again.
        assert_eq!(
            storage
                .propose_dictionary_candidate_from_edit(id, "変更はGitHubにプッシュします")
                .unwrap()
                .outcome,
            EditProposalOutcome::Proposed
        );
    }

    fn propose(storage: &Storage, id: i64) -> EditProposalOutcome {
        storage
            .propose_dictionary_candidate_from_edit(id, "変更はGitHubにプッシュします")
            .unwrap()
            .outcome
    }

    #[test]
    fn single_term_merges_still_count_as_one_change() {
        assert_eq!(
            pair("use open ai today", "use OpenAI today"),
            Some(("open ai".into(), "OpenAI".into()))
        );
        assert!(!changes_several_terms("open ai", "OpenAI"));
        assert!(changes_several_terms(
            "Kubernetis and Dockre",
            "Kubernetes and Docker"
        ));
    }

    #[test]
    fn deleting_learned_data_also_forgets_rejected_and_edited_learning() {
        let (storage, id) = storage_with_history(true);
        // Rejected edit suggestion: the decision must not survive deletion.
        let rejected = storage
            .propose_dictionary_candidate_from_edit(id, "変更はGitHubにプッシュします")
            .unwrap()
            .candidate
            .unwrap();
        assert!(storage.reject_dictionary_candidate(rejected.id).unwrap());
        assert_eq!(propose(&storage, id), EditProposalOutcome::AlreadyKnown);
        // A correction-derived decision is not learning data and stays.
        let correction = storage
            .add_dictionary_candidate_from_correction("open ai", "OpenAI", None)
            .unwrap()
            .unwrap();
        assert!(storage.reject_dictionary_candidate(correction).unwrap());
        storage.delete_learned_personalization().unwrap();
        assert_eq!(propose(&storage, id), EditProposalOutcome::Proposed);
        let decisions: Vec<String> = storage
            .connection()
            .unwrap()
            .prepare("SELECT origin FROM dictionary_candidate_decisions")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(decisions, vec!["correction".to_owned()]);

        // A confirmed learned entry edited afterwards is still learned data.
        let candidate = storage.list_dictionary_candidates().unwrap()[0].clone();
        let entry = storage
            .confirm_dictionary_candidate(candidate.id)
            .unwrap()
            .unwrap();
        let aliases = Vec::new();
        storage
            .update_dictionary_entry(
                entry.id,
                &NewDictionaryEntry {
                    reading: "ぎっとはぶ",
                    surface: "GitHub Enterprise",
                    category: None,
                    aliases: &aliases,
                    priority: 1,
                    app_scope: None,
                },
            )
            .unwrap();
        storage.delete_learned_personalization().unwrap();
        assert!(storage.list_dictionary().unwrap().is_empty());
        assert_eq!(propose(&storage, id), EditProposalOutcome::Proposed);
    }

    #[test]
    fn a_proposal_rechecks_learning_and_history_when_it_stores() {
        // The first check passed, then another command changed settings
        // before the insert: the store step must refuse and write nothing.
        for settings in [
            Settings::default(),
            Settings {
                history_retention: HistoryRetention::Never,
                ..learning_on()
            },
        ] {
            let (storage, id) = storage_with_history(true);
            let (original, preferred) = detect_edit_candidate(
                "変更はぎっとはぶにプッシュします",
                "変更はGitHubにプッシュします",
            )
            .unwrap();
            save(&storage, settings);
            assert!(storage
                .store_edit_proposal(id, &original, &preferred)
                .is_err());
            assert_eq!(
                storage
                    .connection()
                    .unwrap()
                    .query_row("SELECT COUNT(*) FROM dictionary_candidates", [], |row| row
                        .get::<_, i64>(
                        0
                    ))
                    .unwrap(),
                0
            );
        }
        // A History row deleted in between is not eligible either.
        let (storage, id) = storage_with_history(true);
        storage.delete_history(id).unwrap();
        assert!(storage
            .store_edit_proposal(id, "ぎっとはぶ", "GitHub")
            .is_err());
    }

    #[test]
    fn deleting_learned_data_removes_it_from_database_and_wal_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("learned.sqlite3");
        let storage = Storage::open(&path).unwrap();
        save(&storage, learning_on());
        let sentinel = "LearnedSentinelSpelling";
        let aliases = Vec::new();
        storage
            .add_dictionary_entry(&NewDictionaryEntry {
                reading: "sentinel reading",
                surface: sentinel,
                category: None,
                aliases: &aliases,
                priority: 0,
                app_scope: None,
            })
            .unwrap();
        storage
            .connection()
            .unwrap()
            .execute(
                "UPDATE dictionary_entries SET source = ?1",
                [LEARNED_SOURCE],
            )
            .unwrap();
        let files = [path.clone(), directory.path().join("learned.sqlite3-wal")];
        let contains = |bytes: &[u8]| {
            bytes
                .windows(sentinel.len())
                .any(|part| part == sentinel.as_bytes())
        };
        assert!(files
            .iter()
            .any(|file| std::fs::read(file).is_ok_and(|bytes| contains(&bytes))));
        storage.delete_learned_personalization().unwrap();
        for file in files {
            assert!(!std::fs::read(file).is_ok_and(|bytes| contains(&bytes)));
        }
    }

    #[test]
    fn modes_are_validated_and_stored_canonically() {
        assert_eq!(stored_modes(&[]).unwrap(), None);
        assert_eq!(
            stored_modes(&["ask".into(), "dictate".into()])
                .unwrap()
                .as_deref(),
            Some(r#"["dictate","ask"]"#)
        );
        assert_eq!(
            stored_modes(&["ask".into(), "edit".into(), "dictate".into()]).unwrap(),
            None
        );
        assert!(stored_modes(&["translate".into()]).is_err());
        assert_eq!(read_modes(Some("not json")), Vec::<String>::new());
        assert!(mode_matches(&[], DictionaryMode::Ask));
        assert!(!mode_matches(&["dictate".into()], DictionaryMode::Ask));
    }
}
