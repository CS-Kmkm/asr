//! Negation and question checks for provider rewrites (Issue #85).
//!
//! Both are deterministic, surface-level counts. They catch the rewrite errors
//! the offline evaluation showed the earlier guards missed (送らないで ->
//! 送って, "don't merge" -> "merge", a question turned into an answer) and
//! leave semantic paraphrase to human review.
use super::fact_check::{
    find_repair_cue, is_english_word_character, is_sentence_break_at, FactEdits,
};
use unicode_normalization::UnicodeNormalization;

/// Japanese negative forms. None contains another, so counts never overlap.
const JA_NEGATIONS: [&str; 8] = [
    "ない",
    "なかった",
    "なく",
    "ません",
    "せず",
    "無い",
    "無し",
    "なし",
];
/// Phrases whose negative morphemes do not negate the statement: obligation
/// (しなければならない = must), hedges (かもしれない), and spoken repair cues
/// (Aじゃなくて B). They are masked before counting, longest first.
const JA_NON_NEGATIONS: [&str; 22] = [
    "なければなりません",
    "なければいけません",
    "なくてはなりません",
    "なくてはいけません",
    "なければならない",
    "なければいけない",
    "なくてはならない",
    "なくてはいけない",
    "ないといけません",
    "ないといけない",
    "かもしれません",
    "かもしれない",
    "しれません",
    "しれない",
    "ではなくて",
    "じゃなくて",
    "ないとだめ",
    "ではなく",
    "じゃなく",
    "なくちゃ",
    "なきゃ",
    "まもなく",
];
const EN_NEGATIONS: [&str; 10] = [
    "not", "never", "no", "none", "nothing", "nobody", "neither", "nor", "cannot", "without",
];
/// A negation more than this many characters before a repair cue is not
/// part of the repaired phrase.
const REPAIR_REACH_CHARS: usize = 80;

/// NFKC, lower case, typographic apostrophes as ASCII, and non-negating
/// phrases replaced by `#` of the same byte length so offsets still line up
/// with repair cues found in the unmasked text.
fn prepared(text: &str) -> (String, String) {
    let normalized = text
        .nfkc()
        .collect::<String>()
        .to_lowercase()
        .replace('\u{2019}', "'");
    let mut masked = normalized.clone();
    for phrase in JA_NON_NEGATIONS {
        masked = masked.replace(phrase, &"#".repeat(phrase.len()));
    }
    (normalized, masked)
}

/// Start/end byte offsets of every negation in `masked`.
fn negation_spans(masked: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for marker in JA_NEGATIONS {
        let mut from = 0;
        while let Some(at) = masked[from..].find(marker) {
            let start = from + at;
            spans.push((start, start + marker.len()));
            from = start + marker.len();
        }
    }
    for (start, end) in english_words(masked) {
        let word = &masked[start..end];
        let next = masked[end..]
            .chars()
            .find(|character| !character.is_whitespace());
        // "No, ..." and a trailing "no" are answers or interjections.
        let interjection =
            word == "no" && next.is_none_or(|next| matches!(next, ',' | '.' | '!' | '?'));
        if (EN_NEGATIONS.contains(&word) && !interjection) || word.ends_with("n't") {
            spans.push((start, end));
        }
    }
    spans.sort_unstable();
    spans
}

fn english_words(text: &str) -> Vec<(usize, usize)> {
    let mut words = Vec::new();
    let mut start = None;
    for (index, character) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        let word_character =
            is_english_word_character(character) || (character == '\'' && start.is_some());
        match (start, word_character) {
            (None, true) => start = Some(index),
            (Some(begin), false) => {
                let word = text[begin..index].trim_end_matches('\'');
                words.push((begin, begin + word.len()));
                start = None;
            }
            _ => {}
        }
    }
    words
}

/// Whether a later repair cue in the same sentence may replace this span.
fn repaired(text: &str, end: usize) -> bool {
    let tail = &text[end..];
    let Some((cue_at, _)) = find_repair_cue(tail) else {
        return false;
    };
    let before = &tail[..cue_at];
    before.chars().count() <= REPAIR_REACH_CHARS
        && !before
            .char_indices()
            .any(|(index, character)| is_sentence_break_at(before, index, character))
}

/// The output must keep every negation of the source and add none.
///
/// With self-correction on, a negation followed by a nearby repair cue in the
/// same sentence may be dropped (行かない、いや行く). With repetition removal
/// on, repeated identical negated phrases may merge into one.
pub(super) fn negation_preserved(source: &str, output: &str, edits: FactEdits) -> bool {
    let (source_text, source_masked) = prepared(source);
    let (_, output_masked) = prepared(output);
    let source_spans = negation_spans(&source_masked);
    let output_count = negation_spans(&output_masked).len();
    if output_count > source_spans.len() {
        return false;
    }
    let kept = source_spans
        .iter()
        .filter(|(_, end)| !(edits.corrections && repaired(&source_text, *end)))
        .collect::<Vec<_>>();
    let required = if edits.merge_duplicates {
        let mut phrases = kept
            .iter()
            .map(|(start, end)| {
                // The negated phrase: the marker and up to four letters of
                // the word before it (送ら|ないで, should| not).
                let before = &source_masked[..*start];
                let word_end = before.trim_end().len();
                let head = before[..word_end]
                    .char_indices()
                    .rev()
                    .take(4)
                    .take_while(|(_, character)| character.is_alphanumeric())
                    .last()
                    .map_or(word_end, |(index, _)| index);
                &source_masked[head..*end]
            })
            .collect::<Vec<_>>();
        phrases.sort_unstable();
        phrases.dedup();
        phrases.len()
    } else {
        kept.len()
    };
    output_count >= required
}

/// Japanese polite question endings, counted only where a sentence ends.
const JA_QUESTION_ENDINGS: [&str; 4] = ["でしょうか", "ませんか", "ですか", "ますか"];

fn is_question(text: &str) -> bool {
    let text = text.nfkc().collect::<String>();
    text.contains('?')
        || JA_QUESTION_ENDINGS.iter().any(|ending| {
            text.match_indices(ending).any(|(at, _)| {
                text[at + ending.len()..].chars().next().is_none_or(|next| {
                    next.is_whitespace() || matches!(next, '。' | '.' | '!' | '」')
                })
            })
        })
}

/// A spoken question stays a question: an output without any question form
/// is an answer or a statement, not an edit of the transcript.
pub(super) fn question_preserved(source: &str, output: &str) -> bool {
    !is_question(source) || is_question(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: FactEdits = FactEdits {
        corrections: true,
        merge_duplicates: true,
    };
    const NONE: FactEdits = FactEdits {
        corrections: false,
        merge_duplicates: false,
    };

    #[test]
    fn flipped_or_dropped_negations_are_rejected() {
        for (source, output) in [
            ("まだ送らないでください。", "まだ送ってください。"),
            ("中止ではありません。", "中止です。"),
            ("API を呼んでいません。", "API を呼んでいます。"),
            ("please don't merge it", "Please merge it."),
            ("We should not sign it.", "We should sign it."),
            ("Deploy without tests.", "Deploy with tests."),
            ("There are no meetings today.", "There are meetings today."),
        ] {
            assert!(
                !negation_preserved(source, output, ALL),
                "{source} => {output}"
            );
        }
    }

    #[test]
    fn added_negations_are_rejected() {
        assert!(!negation_preserved(
            "送ってください。",
            "送らないでください。",
            ALL
        ));
        assert!(!negation_preserved(
            "申請書は提出しなければなりません。",
            "申請書は提出しなくてもかまいません。",
            ALL
        ));
        assert!(!negation_preserved("Merge it.", "Never merge it.", ALL));
    }

    #[test]
    fn equivalent_wording_and_non_negating_phrases_are_accepted() {
        for (source, output) in [
            ("提出しなければなりません。", "提出する必要があります。"),
            ("雨が降るかもしれない。", "雨が降るかもしれません。"),
            ("14時じゃなくて15時です。", "15時です。"),
            ("まもなく始まります。", "もうすぐ始まります。"),
            ("don't merge", "Do not merge."),
            ("I can't go", "I cannot go."),
            ("No, I mean the second one.", "I mean the second one."),
            ("Don’t merge", "Don't merge."),
            ("使わないでください", "使わないでください。"),
        ] {
            assert!(
                negation_preserved(source, output, NONE),
                "{source} => {output}"
            );
        }
    }

    #[test]
    fn repaired_negations_may_be_dropped_only_with_self_correction() {
        let source = "明日は行かない、いや、やっぱり行きます。";
        assert!(negation_preserved(source, "明日は行きます。", ALL));
        assert!(!negation_preserved(source, "明日は行きます。", NONE));
        // A cue in a later sentence does not repair an earlier negation.
        assert!(!negation_preserved(
            "送らないでください。それと、いや何でもないです。",
            "送ってください。それと、何でもないです。",
            ALL
        ));
    }

    #[test]
    fn repeated_negated_phrases_may_merge_only_with_repetition_removal() {
        let source = "送らないで、送らないでください。";
        let merge = FactEdits {
            corrections: false,
            merge_duplicates: true,
        };
        assert!(negation_preserved(source, "送らないでください。", merge));
        assert!(!negation_preserved(source, "送らないでください。", NONE));
        assert!(!negation_preserved(
            "送らないで、確認しないでください。",
            "送らないで、確認してください。",
            merge
        ));
    }

    #[test]
    fn questions_must_stay_questions() {
        assert!(!question_preserved(
            "この関数ってスレッドセーフですか？",
            "はい、この関数はスレッドセーフです。"
        ));
        assert!(!question_preserved(
            "Is it thread safe?",
            "Yes, it is thread safe."
        ));
        assert!(!question_preserved("明日来ますか", "明日来ます。"));
        assert!(question_preserved("明日来ますか", "明日来ますか？"));
        assert!(question_preserved(
            "この関数ってスレッドセーフですか？",
            "この関数はスレッドセーフですか？"
        ));
        assert!(question_preserved(
            "can you tell me the deadline",
            "The deadline is Friday."
        ));
        assert!(question_preserved("送ります。", "送ります。"));
        // ですか inside a clause is not a sentence-final question.
        assert!(question_preserved(
            "必要ですかどうか確認します。",
            "必要か確認します。"
        ));
    }
}
