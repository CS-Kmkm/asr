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
/// (しなければならない = must), hedges (かもしれない), spoken repair cues
/// (Aじゃなくて B), and affirmative adverbs built on なく (少なくとも = at
/// least, 間もなく = soon). They are masked before counting, longest first
/// where one contains another.
const JA_NON_NEGATIONS: &[&str] = &[
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
    "少なくとも",
    "すくなくとも",
    "まもなく",
    "間もなく",
    "ほどなく",
    "程なく",
    "もれなく",
    "漏れなく",
    "くまなく",
    "隈なく",
    "何気なく",
    "なにげなく",
    "さりげなく",
];
const EN_NEGATIONS: [&str; 10] = [
    "not", "never", "no", "none", "nothing", "nobody", "neither", "nor", "cannot", "without",
];
/// The most text a repaired negated predicate may carry after its negation
/// before the repair cue: a kana auxiliary tail (ないでください) or a few
/// English words (don't merge it, actually ...).
const REPAIR_TAIL_KANA: usize = 6;
const REPAIR_TAIL_WORDS: usize = 3;

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
    for phrase in JA_NON_NEGATIONS.iter().copied() {
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

/// Whether a repair cue directly follows the negated predicate, so the repair
/// replaces that predicate (行かない、いや行きます). Only the predicate's own
/// tail may stand between them: kana auxiliaries or a few English words, with
/// no clause break. A cue that repairs something later in the sentence
/// (送らないで、会議は3時、いや4時) leaves the negation in force.
fn repaired(text: &str, end: usize) -> bool {
    let tail = &text[end..];
    let Some((cue_at, _)) = find_repair_cue(tail) else {
        return false;
    };
    let separator = |c: char| c.is_whitespace() || matches!(c, '、' | ',' | '，');
    let between = tail[..cue_at].trim_matches(separator);
    if between
        .char_indices()
        .any(|(index, c)| (separator(c) && c != ' ') || is_sentence_break_at(between, index, c))
    {
        return false;
    }
    let kana_tail = between.chars().count() <= REPAIR_TAIL_KANA
        && between
            .chars()
            .all(|c| matches!(c, '\u{3041}'..='\u{3096}' | 'ー'));
    let word_tail = between.split_whitespace().count() <= REPAIR_TAIL_WORDS
        && between
            .chars()
            .all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '\'');
    kana_tail || word_tail
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

/// The number of question sentences: sentences ending in `?`, or in a polite
/// Japanese question ending (ですか) without a mark.
fn question_count(text: &str) -> usize {
    let text = text.nfkc().collect::<String>();
    let mut count = 0;
    let mut sentence = String::new();
    for character in text.chars().chain(std::iter::once('\n')) {
        if matches!(character, '。' | '!' | '?' | '\n') {
            let trimmed = sentence.trim_end_matches(|c: char| c.is_whitespace() || c == '」');
            if character == '?'
                || JA_QUESTION_ENDINGS
                    .iter()
                    .any(|ending| trimmed.ends_with(ending))
            {
                count += 1;
            }
            sentence.clear();
        } else {
            sentence.push(character);
        }
    }
    count
}

/// Every spoken question stays a question: an output with fewer question
/// sentences answered or restated one (確認できますか。送信できますか。 ->
/// 確認できますか。送信できます。).
pub(super) fn question_preserved(source: &str, output: &str) -> bool {
    question_count(output) >= question_count(source)
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
    fn an_unrelated_repair_cue_does_not_license_dropping_a_prohibition() {
        assert!(!negation_preserved(
            "本番には送らないで、会議は3時、いや4時です",
            "本番には送って、会議は4時です",
            ALL
        ));
        assert!(!negation_preserved(
            "Don't deploy today, the meeting is at 3, actually 4.",
            "Deploy today, the meeting is at 4.",
            ALL
        ));
        // The cue right after the negated predicate still repairs it.
        assert!(negation_preserved(
            "送らないでください、いや、送ってください",
            "送ってください",
            ALL
        ));
        assert!(negation_preserved(
            "don't merge it, actually merge it",
            "Merge it.",
            ALL
        ));
    }

    #[test]
    fn affirmative_adverbs_built_on_naku_are_not_negations() {
        for (source, output) in [
            ("少なくとも3人必要です", "最低3人必要です"),
            ("間もなく始まります", "もうすぐ始まります"),
            ("ほどなく届きます", "すぐに届きます"),
            ("もれなく確認してください", "すべて確認してください"),
            ("さりげなく伝えます", "控えめに伝えます"),
        ] {
            assert!(
                negation_preserved(source, output, NONE),
                "{source} => {output}"
            );
        }
        // A real negation next to them still counts.
        assert!(!negation_preserved(
            "少なくとも3人は来ない",
            "少なくとも3人は来る",
            NONE
        ));
    }

    #[test]
    fn every_question_sentence_must_stay_a_question() {
        assert!(!question_preserved(
            "確認できますか。送信できますか。",
            "確認できますか。送信できます。"
        ));
        assert!(!question_preserved(
            "Can you check? Can you send it?",
            "Can you check? I can send it."
        ));
        assert!(question_preserved(
            "確認できますか。送信できますか。",
            "確認できますか？送信できますか？"
        ));
        assert_eq!(question_count("必要ですかどうか確認します。"), 0);
        assert_eq!(question_count("いいですか？ 明日来ますか"), 2);
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
