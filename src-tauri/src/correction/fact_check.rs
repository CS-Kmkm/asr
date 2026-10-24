//! Fact preservation independent of provider transport and prompt wording.
use super::{CorrectionError, ProtectedContentKind};
use crate::{
    dictionary::{fold, CorrectionTerm},
    types::Settings,
};
use std::collections::HashMap;
use unicode_normalization::UnicodeNormalization;

fn conservative_output_is_plausible(source: &str, output: &str, terms: &[CorrectionTerm]) -> bool {
    let normalize = |text: &str| fold(&text.nfkc().collect::<String>());
    let mut source = normalize(source);
    let output = normalize(output);
    for term in terms {
        let surface = normalize(&term.surface);
        for reading in &term.readings {
            let reading = normalize(reading);
            if reading.chars().count() >= 3 && !surface.is_empty() {
                source = source.replace(&reading, &surface);
            }
        }
    }
    let content = |text: &str| {
        text.chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<Vec<_>>()
    };
    let source = content(&source);
    let output = content(&output);
    if source == output {
        return true;
    }
    if source.is_empty() || output.is_empty() || output.len() > source.len() * 3 + 8 {
        return false;
    }
    let japanese = |c: &&char| matches!(**c as u32, 0x3040..=0x30ff | 0x3400..=0x9fff);
    let source_ja = source.iter().filter(japanese).count();
    let output_ja = output.iter().filter(japanese).count();
    let source_latin = source.iter().filter(|c| c.is_ascii_alphabetic()).count();
    let output_latin = output.iter().filter(|c| c.is_ascii_alphabetic()).count();
    if (source_ja >= 4 && output_ja * 3 < source_ja)
        || (source_latin >= 4 && output_latin * 3 < source_latin)
    {
        return false;
    }
    let grams = |text: &[char]| {
        let mut counts = HashMap::new();
        for pair in text.windows(2) {
            *counts.entry((pair[0], pair[1])).or_insert(0usize) += 1;
        }
        counts
    };
    if source.len() < 4 || output.len() < 4 {
        // Short commands and questions give too little evidence for a rewrite.
        return source.len() >= 3
            && output.starts_with(&source)
            && output.len() <= source.len() + 4;
    }
    let before = grams(&source);
    let after = grams(&output);
    let common: usize = before
        .iter()
        .map(|(pair, count)| count.min(after.get(pair).unwrap_or(&0)))
        .sum();
    common as f64 / (source.len() - 1) as f64 >= 0.30
        && common as f64 / (output.len() - 1) as f64 >= 0.30
}

/// Decides whether a completed provider result may replace the transcript.
///
/// Conservative output uses a cheap similarity guard; intent-aware output
/// additionally verifies the protected source facts.
pub(super) fn accept_provider_correction(
    settings: &Settings,
    transcript: &str,
    dictionary_hints: &[CorrectionTerm],
    corrected: String,
) -> Result<String, CorrectionError> {
    if settings.correction_mode != "intent_aware" {
        return if conservative_output_is_plausible(transcript, &corrected, dictionary_hints) {
            Ok(corrected)
        } else {
            Err(CorrectionError::ConservativeContentChanged)
        };
    }
    validate_correction_output_with_terms(settings, transcript, &corrected, dictionary_hints)?;
    Ok(corrected)
}

#[cfg(test)]
pub(super) fn validate_correction_output(
    settings: &Settings,
    transcript: &str,
    corrected: &str,
) -> Result<(), CorrectionError> {
    validate_correction_output_with_terms(settings, transcript, corrected, &[])
}

pub(super) fn validate_correction_output_with_terms(
    settings: &Settings,
    transcript: &str,
    corrected: &str,
    dictionary_hints: &[CorrectionTerm],
) -> Result<(), CorrectionError> {
    if settings.correction_mode != "intent_aware" {
        return Ok(());
    }
    let edits = FactEdits {
        corrections: settings.correction_resolve_self_corrections,
        merge_duplicates: settings.correction_remove_repetitions,
    };
    protected_rejection(transcript, corrected, edits, dictionary_hints).map_or(Ok(()), |kind| {
        Err(CorrectionError::ProtectedContentChanged(kind))
    })?;
    Ok(())
}

/// Editing switches that decide which source facts the output may drop.
#[derive(Clone, Copy)]
pub(super) struct FactEdits {
    /// Explicitly superseded values may be dropped or applied globally.
    pub(super) corrections: bool,
    /// Repeated mentions of one value may be merged into a single mention.
    pub(super) merge_duplicates: bool,
}

#[cfg(test)]
pub(super) fn preserves_protected_spans(
    transcript: &str,
    corrected: &str,
    allow_corrections: bool,
) -> bool {
    let edits = FactEdits {
        corrections: allow_corrections,
        merge_duplicates: false,
    };
    preserves_protected_spans_with_terms(transcript, corrected, edits, &[])
}

#[cfg(test)]
pub(super) fn preserves_protected_spans_with_terms(
    transcript: &str,
    corrected: &str,
    edits: FactEdits,
    dictionary_hints: &[CorrectionTerm],
) -> bool {
    protected_rejection(transcript, corrected, edits, dictionary_hints).is_none()
}

fn protected_rejection(
    transcript: &str,
    corrected: &str,
    edits: FactEdits,
    dictionary_hints: &[CorrectionTerm],
) -> Option<ProtectedContentKind> {
    // Ordered-list markers the model adds for formatting are not facts: they
    // may neither count as new values nor stand in for a dropped number.
    let corrected = &strip_ordered_list_markers(corrected);
    let Some(output_for_new_values) =
        remove_prompted_dictionary_surfaces(transcript, corrected, dictionary_hints)
    else {
        return Some(ProtectedContentKind::Name);
    };
    for (kind, reason) in [
        (ProtectedKind::Url, ProtectedContentKind::Url),
        (ProtectedKind::Number, ProtectedContentKind::Number),
        (ProtectedKind::Code, ProtectedContentKind::Code),
    ] {
        if !facts_preserved(transcript, corrected, &output_for_new_values, kind, edits) {
            return Some(reason);
        }
    }
    if !uncertainty_preserved(transcript, corrected) {
        return Some(ProtectedContentKind::Uncertainty);
    }
    if !proper_nouns_preserved_with_terms(transcript, corrected, edits, dictionary_hints) {
        return Some(ProtectedContentKind::Name);
    }
    None
}

/// Names must survive intent-aware editing: every kept source name stays,
/// an explicitly repaired name may be replaced by its repair, and the output
/// may not introduce a name the speaker never said. Comparison ignores case,
/// so capitalization fixes such as Github -> GitHub are accepted, and a name
/// may be mentioned more or fewer times while it still appears.
///
/// Names are Latin-script tokens that look like proper nouns (see
/// [`is_latin_proper_noun`]) and prompted dictionary terms, the only way a
/// Japanese name is known to be a name.
pub(super) fn proper_nouns_preserved_with_terms(
    source: &str,
    output: &str,
    edits: FactEdits,
    dictionary_hints: &[CorrectionTerm],
) -> bool {
    // Normalize only the name comparison. Number, URL and code checks keep
    // their existing spelling-sensitive contract.
    let normalized_source: String = source.nfkc().collect();
    let normalized_output: String = output.nfkc().collect();
    let (source, output) = (normalized_source.as_str(), normalized_output.as_str());
    let terms: Vec<_> = dictionary_terms(dictionary_hints)
        .into_iter()
        .map(|(surface, readings)| {
            (
                surface.nfkc().collect::<String>(),
                readings
                    .iter()
                    .map(|reading| reading.nfkc().collect::<String>())
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let occurrences = name_occurrences(source, &terms);
    let roles = occurrence_roles(source, &occurrences, edits.corrections);
    let present = |key: &FactKey| {
        terms
            .iter()
            .find(|(surface, _)| fold(surface) == key.value)
            .map_or_else(
                || contains_name(output, &key.value),
                |(surface, readings)| {
                    term_spellings(surface, readings)
                        .any(|spelling| contains_name(output, &fold(spelling)))
                },
            )
    };
    let kept = occurrences
        .iter()
        .zip(&roles)
        .all(|((_, key), role)| match role {
            OccurrenceRole::Kept => present(key),
            OccurrenceRole::Superseded => true,
            OccurrenceRole::Replaceable(final_key) => present(key) || present(final_key),
        });
    // Prompted dictionary spellings are trusted; any other name must have
    // been spoken. Whole tokens are skipped, so a surface such as "Git" never
    // turns "GitHub" into an unknown "Hub".
    let term_ranges = name_occurrences(output, &terms)
        .into_iter()
        .filter(|(_, key)| terms.iter().any(|(surface, _)| fold(surface) == key.value))
        .map(|(range, _)| range)
        .collect::<Vec<_>>();
    kept && latin_name_occurrences(output)
        .iter()
        .filter(|(range, _)| !term_ranges.iter().any(|term| ranges_overlap(term, range)))
        .all(|(_, name)| contains_name(source, &fold(name)))
}

/// The spellings that identify a prompted term. Readings in scripts without
/// word separators must be at least three characters, since a short reading
/// such as あい also occurs inside unrelated words (ぐあい).
pub(super) fn term_spellings<'a>(
    surface: &'a str,
    readings: &'a [String],
) -> impl Iterator<Item = &'a str> {
    std::iter::once(surface).chain(
        readings
            .iter()
            .map(String::as_str)
            .filter(|reading| reading.is_ascii() || reading.chars().count() >= 3),
    )
}

/// Prompted dictionary terms as `(surface, spoken readings)`.
pub(super) fn dictionary_terms(terms: &[CorrectionTerm]) -> Vec<(String, Vec<String>)> {
    terms
        .iter()
        .filter(|term| !term.surface.is_empty())
        .map(|term| (term.surface.clone(), term.readings.clone()))
        .collect()
}

/// Name occurrences in source order. A prompted term is keyed by its surface,
/// whichever spelling was spoken.
pub(super) fn name_occurrences(text: &str, terms: &[(String, Vec<String>)]) -> Vec<FactOccurrence> {
    let mut candidates: Vec<FactOccurrence> = Vec::new();
    let folded = Folded::new(text);
    for (surface, readings) in terms {
        let key = FactKey {
            value: fold(surface),
            unit: None,
        };
        for spelling in term_spellings(surface, readings) {
            for range in folded.matches(&fold(spelling)) {
                if name_boundaries(text, &range) {
                    candidates.push((range, key.clone()));
                }
            }
        }
    }
    // The longest spelling wins where terms overlap: "git hub" is GitHub,
    // not Git followed by "hub".
    candidates.sort_by_key(|(range, _)| (std::cmp::Reverse(range.len()), range.start));
    let mut occurrences: Vec<FactOccurrence> = Vec::new();
    for (range, key) in candidates {
        if !occurrences
            .iter()
            .any(|(existing, _)| ranges_overlap(existing, &range))
        {
            occurrences.push((range, key));
        }
    }
    for (range, name) in latin_name_occurrences(text) {
        if !occurrences
            .iter()
            .any(|(existing, _)| ranges_overlap(existing, &range))
        {
            occurrences.push((
                range,
                FactKey {
                    value: fold(&name),
                    unit: None,
                },
            ));
        }
    }
    occurrences.sort_by_key(|(range, _)| range.start);
    occurrences
}

pub(super) fn ranges_overlap(
    left: &std::ops::Range<usize>,
    right: &std::ops::Range<usize>,
) -> bool {
    left.start < right.end && right.start < left.end
}

/// ASCII names need word boundaries; other scripts have no word separators.
pub(super) fn name_boundaries(text: &str, range: &std::ops::Range<usize>) -> bool {
    if !text[range.clone()].is_ascii() {
        return true;
    }
    let before = text[..range.start].chars().next_back();
    let after = text[range.end..].chars().next();
    !before.is_some_and(|character| character.is_ascii_alphanumeric())
        && !after.is_some_and(|character| character.is_ascii_alphanumeric())
}

/// `text` folded, with each folded byte offset that begins a source
/// character's folding mapped back to that character's offset in `text`.
/// Case mapping can change lengths (the Kelvin sign shrinks, a dotted capital
/// I grows), so folded offsets are never reused directly on `text`.
pub(super) struct Folded {
    folded: String,
    to_source: Vec<Option<usize>>,
}

impl Folded {
    fn new(text: &str) -> Self {
        let mut folded = String::with_capacity(text.len());
        let mut to_source = Vec::with_capacity(text.len() + 1);
        for (at, character) in text.char_indices() {
            to_source.push(Some(at));
            folded.extend(character.to_lowercase());
            to_source.resize(folded.len(), None);
        }
        to_source.push(Some(text.len()));
        Self { folded, to_source }
    }

    /// Source ranges of `name` (already folded). A match that starts or ends
    /// inside one character's folding is not a match of that character.
    fn matches<'a>(&'a self, name: &'a str) -> impl Iterator<Item = std::ops::Range<usize>> + 'a {
        self.folded
            .match_indices(name)
            .filter(|_| !name.is_empty())
            .filter_map(|(at, _)| Some(self.to_source[at]?..self.to_source[at + name.len()]?))
    }
}

/// Whether `text` mentions `name` (folded), as a whole word when ASCII.
pub(super) fn contains_name(text: &str, name: &str) -> bool {
    !name.is_empty()
        && Folded::new(text)
            .matches(name)
            .any(|range| name_boundaries(text, &range))
}

/// Latin-script tokens that look like proper nouns, outside URLs and code.
pub(super) fn latin_name_occurrences(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let excluded = url_occurrences(text)
        .into_iter()
        .chain(code_span_occurrences(text))
        .map(|(range, _)| range)
        .collect::<Vec<_>>();
    let bytes = text.as_bytes();
    let mut names = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_alphanumeric() {
            index += 1;
            continue;
        }
        let start = index;
        let mut end = index;
        while end < bytes.len() {
            if bytes[end].is_ascii_alphanumeric() {
                end += 1;
            } else if matches!(bytes[end], b'.' | b'-' | b'_' | b'\'')
                && bytes.get(end + 1).is_some_and(u8::is_ascii_alphanumeric)
            {
                end += 2;
            } else {
                break;
            }
        }
        while end < bytes.len() && matches!(bytes[end], b'+' | b'#') {
            end += 1;
        }
        index = end;
        // A possessive belongs to the sentence, not the name: Tanaka's.
        let token = &bytes[start..end];
        if token.len() > 2 && (token.ends_with(b"'s") || token.ends_with(b"'S")) {
            end -= 2;
        }
        let range = start..end;
        if excluded.iter().any(|other| ranges_overlap(other, &range)) {
            continue;
        }
        let token = &text[range.clone()];
        let followed_by_non_latin_word = text[end..]
            .chars()
            .next()
            .is_some_and(|c| !c.is_ascii() && c.is_alphabetic());
        if is_latin_proper_noun(
            token,
            sentence_initial(text, start) && !followed_by_non_latin_word,
        ) {
            names.push((range, token.to_owned()));
        }
    }
    names
}

/// A token is a name when it has an inner capital (GitHub, iPhone, NASA) or
/// is capitalized away from a sentence start (Tanaka, Zoom). A capitalized
/// sentence start is ambiguous and ignored. Single letters, "I" contractions
/// and "OK" are ordinary words.
pub(super) fn is_latin_proper_noun(token: &str, sentence_initial: bool) -> bool {
    let letters = token.chars().filter(char::is_ascii_alphabetic).count();
    if letters < 2 || token.eq_ignore_ascii_case("ok") || token.starts_with("I'") {
        return false;
    }
    let mut characters = token.chars();
    let first = characters.next().unwrap_or_default();
    let inner_capital = characters.any(|character| character.is_ascii_uppercase());
    inner_capital || first.is_ascii_uppercase() && !sentence_initial
}

pub(super) fn sentence_initial(text: &str, start: usize) -> bool {
    let before = text[..start].trim_end_matches(|character: char| {
        character.is_whitespace() || matches!(character, '"' | '\'' | '(' | '「' | '『' | '（')
    });
    match before.char_indices().next_back() {
        None => true,
        Some((index, character)) => {
            character == '\n'
                || matches!(character, ':' | '：' | '-' | '*' | '・')
                || is_sentence_break_at(before, index, character)
        }
    }
}

/// Removes a line-leading `1.` or `1)` marker (one or two ASCII digits followed
/// by whitespace) from every line.
pub(super) fn strip_ordered_list_markers(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let body = line.trim_start_matches([' ', '\t']);
            let indent = &line[..line.len() - body.len()];
            let digits = body.bytes().take_while(u8::is_ascii_digit).count();
            let rest = &body[digits..];
            let marker = (1..=2).contains(&digits)
                && rest.starts_with(['.', ')'])
                && rest[1..].starts_with([' ', '\t']);
            if marker {
                format!("{indent}{}", &rest[1..])
            } else {
                line.to_owned()
            }
        })
        .collect()
}

/// Removes trusted dictionary spellings before new values are counted. Returns
/// `None` when a surface appears more often than its spoken readings allow.
pub(super) fn remove_prompted_dictionary_surfaces(
    transcript: &str,
    corrected: &str,
    dictionary_hints: &[CorrectionTerm],
) -> Option<String> {
    let mut without_dictionary_surfaces = corrected.to_owned();
    for term in dictionary_hints {
        let surface = fold(&term.surface);
        if surface.is_empty() {
            continue;
        }
        let source = Folded::new(transcript);
        let mut occurrences = term
            .readings
            .iter()
            .flat_map(|reading| source.matches(&fold(reading)).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        if occurrences.is_empty() {
            return None;
        }
        occurrences.extend(source.matches(&surface));
        // A reading and its aliases may cover the same spoken occurrence.
        // Prefer the longest match at each start, then count only disjoint
        // source spans, including already-correct spellings.
        occurrences.sort_by_key(|range| (range.start, std::cmp::Reverse(range.end)));
        let mut end = 0;
        let allowed = occurrences
            .into_iter()
            .filter(|range| {
                if range.start < end {
                    false
                } else {
                    end = range.end;
                    true
                }
            })
            .count();
        let corrected_folded = Folded::new(corrected);
        if corrected_folded.matches(&surface).count() > allowed {
            return None;
        }
        let ranges = Folded::new(&without_dictionary_surfaces)
            .matches(&surface)
            .collect::<Vec<_>>();
        for range in ranges.into_iter().rev() {
            without_dictionary_surfaces.replace_range(range, "");
        }
    }
    Some(without_dictionary_surfaces)
}

#[derive(Clone, Copy)]
pub(super) enum ProtectedKind {
    Url,
    Number,
    Code,
}

/// A protected value. Numbers keep their counter so 3人 and 3円 differ.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct FactKey {
    value: String,
    unit: Option<char>,
}

pub(super) type FactOccurrence = (std::ops::Range<usize>, FactKey);

/// How the output may treat one source occurrence, decided by its position.
#[derive(Clone, PartialEq)]
pub(super) enum OccurrenceRole {
    /// The value must survive.
    Kept,
    /// An explicit repair cue replaced this occurrence; it may be dropped.
    Superseded,
    /// An earlier mention of a value that a later repair replaced. A global
    /// correction may turn it into the final replacement instead of keeping it.
    Replaceable(FactKey),
}

pub(super) fn fact_occurrences(text: &str, kind: ProtectedKind) -> Vec<FactOccurrence> {
    let plain =
        |(range, value): (std::ops::Range<usize>, String)| (range, FactKey { value, unit: None });
    match kind {
        ProtectedKind::Number => number_occurrences(text)
            .into_iter()
            .map(|(range, value)| {
                let unit = number_unit(text, &range);
                (range, FactKey { value, unit })
            })
            .collect(),
        ProtectedKind::Url => url_occurrences(text).into_iter().map(plain).collect(),
        ProtectedKind::Code => code_span_occurrences(text).into_iter().map(plain).collect(),
    }
}

/// Returns the counter or unit after a number, with equivalent counters
/// (人/名, 回/度, つ/個) mapped to one representative.
pub(super) fn number_unit(text: &str, range: &std::ops::Range<usize>) -> Option<char> {
    let unit = text[range.end..]
        .chars()
        .next()
        .filter(|character| is_counter_character(*character))?;
    Some(match unit {
        '名' => '人',
        '度' => '回',
        '個' => 'つ',
        other => other,
    })
}

pub(super) fn is_counter_character(character: char) -> bool {
    matches!(
        character,
        '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'
    ) || ('ァ'..='ヺ').contains(&character)
        || character == 'つ'
}

/// Matches source occurrences by position: each one is kept, superseded by a
/// nearby explicit repair, or (before a later repair of the same value)
/// replaceable by that repair's final value. The output must retain every kept
/// occurrence and may not contain a value more often than the source allows.
pub(super) fn facts_preserved(
    source: &str,
    output: &str,
    output_for_new_values: &str,
    kind: ProtectedKind,
    edits: FactEdits,
) -> bool {
    let occurrences = fact_occurrences(source, kind);
    let roles = occurrence_roles(source, &occurrences, edits.corrections);
    let count_keys = |text: &str| {
        let mut counts = HashMap::<FactKey, usize>::new();
        for (_, key) in fact_occurrences(text, kind) {
            *counts.entry(key).or_default() += 1;
        }
        counts
    };

    // No new facts: a value may appear as often as in the source, plus once
    // for each earlier mention that a later correction may replace with it.
    for (key, count) in count_keys(output_for_new_values) {
        let in_source = occurrences
            .iter()
            .filter(|(_, value)| *value == key)
            .count();
        let replaced_into = roles
            .iter()
            .filter(
                |role| matches!(role, OccurrenceRole::Replaceable(final_key) if *final_key == key),
            )
            .count();
        if count > in_source + replaced_into {
            return false;
        }
    }

    let mut available = count_keys(output);
    let mut kept = HashMap::<&FactKey, usize>::new();
    for ((_, key), role) in occurrences.iter().zip(&roles) {
        if *role == OccurrenceRole::Kept {
            *kept.entry(key).or_default() += 1;
        }
    }
    for (key, required) in kept {
        let found = available.get(key).copied().unwrap_or_default();
        if edits.merge_duplicates {
            // Merged duplicates must still leave one mention of the value.
            if found == 0 {
                return false;
            }
        } else if found < required {
            return false;
        } else {
            available.insert(key.clone(), found - required);
        }
    }
    for ((_, key), role) in occurrences.iter().zip(&roles) {
        let OccurrenceRole::Replaceable(final_key) = role else {
            continue;
        };
        let original = available.get(key).copied().unwrap_or_default();
        let replacement = available.get(final_key).copied().unwrap_or_default();
        if edits.merge_duplicates {
            if original == 0 && replacement == 0 {
                return false;
            }
        } else if original > 0 {
            available.insert(key.clone(), original - 1);
        } else if replacement > 0 {
            available.insert(final_key.clone(), replacement - 1);
        } else {
            return false;
        }
    }
    true
}

pub(super) fn occurrence_roles(
    source: &str,
    occurrences: &[FactOccurrence],
    allow_corrections: bool,
) -> Vec<OccurrenceRole> {
    let mut roles = vec![OccurrenceRole::Kept; occurrences.len()];
    if !allow_corrections {
        return roles;
    }
    let replacements = (0..occurrences.len())
        .map(|index| replacement_index(source, occurrences, index))
        .collect::<Vec<_>>();
    for index in 0..occurrences.len() {
        let Some(mut target) = replacements[index] else {
            continue;
        };
        roles[index] = OccurrenceRole::Superseded;
        // Replacements always lie later in the transcript, so following
        // successive revisions terminates at the final explicit choice.
        while let Some(next) = replacements[target] {
            target = next;
        }
        let final_key = &occurrences[target].1;
        for earlier in 0..index {
            if roles[earlier] == OccurrenceRole::Kept
                && occurrences[earlier].1 == occurrences[index].1
            {
                roles[earlier] = OccurrenceRole::Replaceable(final_key.clone());
            }
        }
    }
    roles
}

/// Returns the occurrence that explicitly replaces `occurrences[index]`, if any.
pub(super) fn replacement_index(
    source: &str,
    occurrences: &[FactOccurrence],
    index: usize,
) -> Option<usize> {
    let (old_range, old) = &occurrences[index];
    let tail_start = old_range.end;
    let tail_end = occurrences[index + 1..]
        .iter()
        .find(|(_, key)| key.value == old.value)
        .map_or(source.len(), |(range, _)| range.start);
    let tail = &source[tail_start..tail_end];
    let source_clause = source[..old_range.start]
        .rsplit(['。', '、', ',', '.', '!', '?', '！', '？'])
        .next()
        .unwrap_or_default()
        .trim();
    // Only a nearby repair cue with a replacement in its first clause can
    // license dropping a value. A value in a later topic is not a repair.
    let (cue_at, cue_len) = find_repair_cue(tail)?;
    let before_cue = &tail[..cue_at];
    let sentence_breaks = before_cue
        .char_indices()
        .filter(|(index, character)| is_sentence_break_at(before_cue, *index, *character))
        .collect::<Vec<_>>();
    if before_cue.chars().count() > 80 || sentence_breaks.len() > 2 {
        return None;
    }
    if let Some((last_break, character)) = sentence_breaks.last() {
        let after_break = &before_cue[*last_break + character.len_utf8()..];
        if !after_break.trim().is_empty() {
            return None;
        }
    }
    // A second same-kind value before the cue makes its target ambiguous.
    let cue_start = tail_start + cue_at;
    if occurrences.iter().any(|(range, key)| {
        range.start >= tail_start && range.start < cue_start && key.value != old.value
    }) {
        return None;
    }
    let repair = skip_repair_separators_and_fillers(&tail[cue_at + cue_len..]);
    let repair_start = tail_end - repair.len();
    let limit = repair
        .char_indices()
        .nth(80)
        .map_or(repair.len(), |(at, _)| at);
    let clause = &repair[..limit];
    let clause_end = repair_start
        + clause
            .char_indices()
            .find(|(index, character)| {
                is_sentence_break_at(clause, *index, *character) || matches!(character, '、' | ',')
            })
            .map_or(clause.len(), |(index, _)| index);
    let (replacement_index, (replacement_range, replacement)) = occurrences
        .iter()
        .enumerate()
        .skip(index + 1)
        .find(|(_, (range, _))| range.start >= repair_start && range.start < clause_end)?;
    if replacement.value == old.value {
        return None;
    }
    let prefix = source[repair_start..replacement_range.start].trim();
    if !prefix.is_empty() && !source_clause.ends_with(prefix) {
        return None;
    }
    if old.unit.is_some() && replacement.unit.is_some() && old.unit != replacement.unit {
        return None;
    }
    Some(replacement_index)
}

// Longer cues precede their prefixes so ties resolve to the full cue.
const REPAIR_CUES: [&str; 12] = [
    "いや",
    "ではなくて",
    "ではなく",
    "じゃなくて",
    "じゃなく",
    "違う",
    "訂正",
    "正しくは",
    "actually",
    "I meant",
    "I mean",
    "rather",
];
// Empty hesitations that may sit between a repair cue and its replacement.
const REPAIR_FILLERS: [&str; 11] = [
    "えーっと",
    "えーと",
    "ええと",
    "えっと",
    "えー",
    "あのー",
    "あの",
    "うーん",
    "um",
    "uh",
    "er",
];

/// Finds the earliest repair cue in `text` as `(byte offset, byte length)`.
/// English cues match case-insensitively on word boundaries.
pub(super) fn find_repair_cue(text: &str) -> Option<(usize, usize)> {
    REPAIR_CUES
        .iter()
        .filter_map(|cue| {
            let at = if cue.is_ascii() {
                find_english_phrase(text, cue)
            } else {
                text.find(cue)
            };
            at.map(|at| (at, cue.len()))
        })
        .min_by_key(|(at, length)| (*at, std::cmp::Reverse(*length)))
}

pub(super) fn skip_repair_separators_and_fillers(text: &str) -> &str {
    let mut rest = text;
    loop {
        rest = rest.trim_start_matches(|c: char| {
            c.is_whitespace() || matches!(c, '、' | ',' | ':' | '：')
        });
        let filler = REPAIR_FILLERS.iter().find(|filler| {
            rest.get(..filler.len()).is_some_and(|candidate| {
                if filler.is_ascii() {
                    candidate.eq_ignore_ascii_case(filler)
                        && english_word_boundary_after(rest, filler.len())
                } else {
                    candidate == **filler
                }
            })
        });
        match filler {
            Some(filler) => rest = &rest[filler.len()..],
            None => return rest,
        }
    }
}

pub(super) fn is_sentence_break_at(text: &str, index: usize, character: char) -> bool {
    if character == '.' {
        let before = text[..index].chars().next_back();
        let after = text[index + 1..].chars().next();
        return !(before.is_some_and(|value| value.is_ascii_alphanumeric())
            && after.is_some_and(|value| value.is_ascii_alphanumeric()));
    }
    matches!(character, '。' | '!' | '?' | '！' | '？')
}

pub(super) fn code_span_occurrences(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut spans = Vec::new();
    let mut cursor = 0;
    while let Some(relative_start) = text[cursor..].find('`') {
        let start = cursor + relative_start;
        let delimiter_length = text[start..]
            .bytes()
            .take_while(|byte| *byte == b'`')
            .count();
        let delimiter = "`".repeat(delimiter_length);
        let content_start = start + delimiter_length;
        let Some(relative_end) = text[content_start..].find(&delimiter) else {
            break;
        };
        let end = content_start + relative_end + delimiter_length;
        spans.push((start..end, text[start..end].to_owned()));
        cursor = end;
    }
    spans
}

#[cfg(test)]
pub(super) fn extract_code_spans(text: &str) -> Vec<String> {
    let mut spans = Vec::new();
    for (_, span) in code_span_occurrences(text) {
        push_unique_span(&mut spans, &span);
    }
    spans
}

#[cfg(test)]
pub(super) fn extract_numbers(text: &str) -> Vec<String> {
    let mut numbers = Vec::new();
    for (_, value) in number_occurrences(text) {
        push_unique_span(&mut numbers, &value);
    }
    numbers
}

pub(super) fn number_occurrences(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let url_ranges = url_occurrences(text)
        .into_iter()
        .map(|(range, _)| range)
        .collect::<Vec<_>>();
    let mut spans = Vec::new();
    let mut start = None;
    for (index, character) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        let numeric = is_number_character(character);
        let separator = matches!(character, ':' | '.' | ',' | '/' | '-')
            && start.is_some()
            && text[index + character.len_utf8()..]
                .chars()
                .next()
                .is_some_and(is_number_character);
        if numeric || separator {
            if start.is_none() {
                let mut prefix = text[..index]
                    .rfind(|c: char| !is_english_word_character(c))
                    .map_or(0, |at| at + text[at..].chars().next().unwrap().len_utf8());
                if let Some((sign_at, sign)) = text[..index].char_indices().next_back() {
                    if matches!(sign, '-' | '+') && sign_at < prefix {
                        prefix = sign_at;
                    }
                }
                let before = text[..index].trim_end_matches([' ', '\t']);
                if index - before.len() <= 2 {
                    if let Some((sign_at, sign)) = before.char_indices().next_back() {
                        if matches!(sign, '-' | '+') && sign_at < prefix {
                            prefix = sign_at;
                        }
                    }
                }
                start = Some(prefix);
            }
        } else if start.is_some() && is_english_word_character(character) {
            continue;
        } else if let Some(begin) = start.take() {
            let number = &text[begin..index];
            if !url_ranges.iter().any(|range| range.contains(&begin))
                && (!is_kanji_numeral_only(number)
                    || kanji_numeral_in_numeric_context(text, begin..index))
            {
                let normalized = if matches!(number.chars().next(), Some('+' | '-')) {
                    let mut chars = number.chars();
                    let sign = chars.next().unwrap();
                    format!("{sign}{}", chars.as_str().trim_start_matches([' ', '\t']))
                } else {
                    number.to_owned()
                };
                spans.push((begin..index, normalized));
            }
        }
    }
    spans
}

const KANJI_NUMERALS: &str = "〇零一二三四五六七八九十百千万億兆壱弐参";
// Counters that make a lone kanji numeral a number. 番, 緒, and 旦 are
// deliberately absent so 一番, 一緒, and 一旦 stay ordinary words.
const KANJI_NUMERAL_COUNTERS: &str =
    "人名回度つ個時分秒日月年円件本枚台冊階歳才週割倍点位号杯匹頭羽曲社店泊軒票行列章節条項期席箇ヶヵかカケ桁";
// Compounds whose lone numeral is idiomatic rather than a count.
const KANJI_NUMERAL_IDIOMS: [&str; 2] = ["十分", "一時的"];

pub(super) fn is_number_character(character: char) -> bool {
    character.is_numeric() || KANJI_NUMERALS.contains(character)
}

pub(super) fn is_kanji_numeral_only(token: &str) -> bool {
    !token
        .chars()
        .any(|character| character.is_numeric() && !KANJI_NUMERALS.contains(character))
}

/// Kanji numerals count as numbers only next to other numerals or before a
/// counter, so words such as 一緒, 一番, and 一旦 are not protected numbers.
pub(super) fn kanji_numeral_in_numeric_context(text: &str, range: std::ops::Range<usize>) -> bool {
    let numerals = text[range.clone()]
        .chars()
        .filter(|character| KANJI_NUMERALS.contains(*character))
        .count();
    if numerals >= 2 {
        return true;
    }
    text[range.end..]
        .chars()
        .next()
        .is_some_and(|counter| KANJI_NUMERAL_COUNTERS.contains(counter))
        && !KANJI_NUMERAL_IDIOMS
            .iter()
            .any(|idiom| text[range.start..].starts_with(idiom))
}

pub(super) fn uncertainty_preserved(source: &str, output: &str) -> bool {
    let english = [
        "maybe",
        "perhaps",
        "possibly",
        "uncertain",
        "not sure",
        "i think",
        "i guess",
    ];
    for marker in english {
        if find_english_uncertainty_marker(source, marker).is_some()
            && find_english_uncertainty_marker(output, marker).is_none()
        {
            return false;
        }
    }
    for variants in [
        &["たぶん", "多分"][..],
        &["おそらく", "恐らく"][..],
        &["かもしれない", "かもしれません", "かも"][..],
        &["不明"][..],
        &["わからない", "分からない", "わかりません", "分かりません"][..],
        &["と思う", "と思います"][..],
    ] {
        if variants
            .iter()
            .any(|marker| contains_japanese_uncertainty(source, marker))
            && !variants
                .iter()
                .any(|marker| contains_japanese_uncertainty(output, marker))
        {
            return false;
        }
    }
    true
}

pub(super) fn contains_japanese_uncertainty(text: &str, marker: &str) -> bool {
    text.match_indices(marker).any(|(at, _)| {
        // しかも ("moreover") contains かも but expresses no uncertainty.
        !(marker == "かも" && text[..at].ends_with('し'))
    })
}

#[cfg(test)]
pub(super) fn protected_spans(transcript: &str) -> Vec<String> {
    let mut spans = Vec::new();
    for url in extract_urls(transcript) {
        push_unique_span(&mut spans, &url);
    }
    for number in extract_numbers(transcript) {
        push_unique_span(&mut spans, &number);
    }
    for code in extract_code_spans(transcript) {
        push_unique_span(&mut spans, &code);
    }

    for marker in [
        "maybe",
        "perhaps",
        "possibly",
        "uncertain",
        "not sure",
        "i think",
        "i guess",
    ] {
        if let Some(found) = find_english_uncertainty_marker(transcript, marker) {
            push_unique_span(&mut spans, found);
        }
    }
    for marker in [
        "たぶん",
        "多分",
        "おそらく",
        "恐らく",
        "かもしれない",
        "かも",
        "不明",
        "わからない",
        "分からない",
        "と思う",
    ] {
        if contains_japanese_uncertainty(transcript, marker) {
            push_unique_span(&mut spans, marker);
        }
    }
    spans
}

#[cfg(test)]
pub(super) fn extract_urls(text: &str) -> Vec<String> {
    url_occurrences(text)
        .into_iter()
        .map(|(_, url)| url)
        .collect()
}

pub(super) fn url_occurrences(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut urls = Vec::new();
    let mut cursor = 0;
    while cursor < text.len() {
        let remaining = &text[cursor..];
        let scheme_length = if remaining
            .as_bytes()
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"https://"))
        {
            "https://".len()
        } else if remaining
            .as_bytes()
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"http://"))
        {
            "http://".len()
        } else {
            let character_length = remaining
                .chars()
                .next()
                .expect("cursor is within the source text")
                .len_utf8();
            cursor += character_length;
            continue;
        };
        let end = remaining
            .char_indices()
            .find(|(index, character)| *index >= scheme_length && !is_url_character(*character))
            .map_or(text.len(), |(index, _)| cursor + index);
        let url = trim_url_sentence_delimiter(&text[cursor..end]);
        if url.len() > scheme_length {
            urls.push((cursor..cursor + url.len(), url.to_string()));
        }
        cursor = end.max(cursor + scheme_length);
    }
    urls
}

pub(super) fn is_url_character(character: char) -> bool {
    character.is_ascii()
        && !character.is_whitespace()
        && !character.is_control()
        && !matches!(
            character,
            '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}'
        )
}

pub(super) fn trim_url_sentence_delimiter(url: &str) -> &str {
    url.trim_end_matches(['.', '!', '?', ',', ';', ':'])
}

pub(super) fn find_english_uncertainty_marker<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    find_english_phrase(text, marker).map(|start| &text[start..start + marker.len()])
}

/// Finds an ASCII phrase case-insensitively on English word boundaries.
pub(super) fn find_english_phrase(text: &str, phrase: &str) -> Option<usize> {
    text.char_indices().map(|(start, _)| start).find(|start| {
        text.get(*start..)
            .and_then(|remaining| remaining.get(..phrase.len()))
            .is_some_and(|candidate| {
                candidate.eq_ignore_ascii_case(phrase)
                    && english_word_boundary_before(text, *start)
                    && english_word_boundary_after(text, start + phrase.len())
            })
    })
}

pub(super) fn english_word_boundary_before(text: &str, index: usize) -> bool {
    text[..index]
        .chars()
        .next_back()
        .is_none_or(|character| !is_english_word_character(character))
}

pub(super) fn english_word_boundary_after(text: &str, index: usize) -> bool {
    text[index..]
        .chars()
        .next()
        .is_none_or(|character| !is_english_word_character(character))
}

pub(super) fn is_english_word_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

#[cfg(test)]
pub(super) fn push_unique_span(spans: &mut Vec<String>, span: &str) {
    if !span.is_empty() && !spans.iter().any(|existing| existing == span) {
        spans.push(span.into());
    }
}
