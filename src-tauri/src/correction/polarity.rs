//! Negation and question checks for provider rewrites (Issue #85).
//!
//! Both are deterministic, surface-level counts. They catch the rewrite errors
//! the offline evaluation showed the earlier guards missed (送らないで ->
//! 送って, "don't merge" -> "merge", a question turned into an answer) and
//! leave semantic paraphrase to human review.
use super::fact_check::{
    find_repair_cue, is_english_word_character, is_sentence_break_at,
    skip_repair_separators_and_fillers, FactEdits,
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

/// The negated predicate a repair must restate: for Japanese the last kanji
/// of the verb before the negation (送|らない -> 送), for English the word
/// after it (don't |merge). `None` when there is no such anchor (しない),
/// so the negation cannot be repaired away.
fn negated_predicate(text: &str, start: usize, end: usize) -> Option<String> {
    let marker = &text[start..end];
    if marker.is_ascii() {
        let after = text[end..].trim_start();
        let word: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        return (word.len() >= 2).then_some(word);
    }
    text[..start]
        .chars()
        .rev()
        .take(3)
        .take_while(|c| !c.is_whitespace() && !matches!(c, '、' | '。' | ',' | '.'))
        .find(|c| matches!(c, '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}'))
        .map(String::from)
}

/// Whether a repair cue directly follows the negated predicate and the
/// repair restates that predicate, so it replaces it (行かない、いや行きます;
/// don't merge it, actually merge it). Only the predicate's own tail may
/// stand between negation and cue: kana auxiliaries or a few English words,
/// with no clause break. A cue that repairs something else (送らないで、
/// 会議は3時、いや4時; 送らないであした、いやあさって確認して) leaves the
/// negation in force.
fn repaired(text: &str, start: usize, end: usize) -> bool {
    let Some(predicate) = negated_predicate(text, start, end) else {
        return false;
    };
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
    if !(kana_tail || word_tail) {
        return false;
    }
    // The repair clause: from after the cue to the next clause break.
    let (cue_at, cue_len) = find_repair_cue(tail).unwrap_or_default();
    let repair = skip_repair_separators_and_fillers(&tail[cue_at + cue_len..]);
    let clause_end = repair
        .char_indices()
        .find(|(index, c)| matches!(c, '、' | ',') || is_sentence_break_at(repair, *index, *c))
        .map_or(repair.len(), |(index, _)| index);
    let clause = &repair[..clause_end];
    if predicate.is_ascii() {
        english_words(clause)
            .into_iter()
            .any(|(word_start, word_end)| clause[word_start..word_end] == predicate)
    } else {
        clause.contains(&predicate)
    }
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
        .filter(|(start, end)| !(edits.corrections && repaired(&source_text, *start, *end)))
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

/// A sentence and whether it is a question: it ends in `?`, or in a polite
/// Japanese question ending (ですか) without a mark.
struct Sentence {
    content: Vec<char>,
    question: bool,
}

/// NFKC sentences split at 。 ! ? line breaks and at a `.` followed by
/// whitespace or the end (so 3.12 and example.com stay whole).
fn sentences(text: &str) -> Vec<Sentence> {
    let text = text.nfkc().collect::<String>();
    let characters = text.chars().collect::<Vec<_>>();
    let mut sentences = Vec::new();
    let mut current = String::new();
    for (index, character) in characters.iter().copied().enumerate() {
        let next = characters.get(index + 1).copied();
        let boundary = matches!(character, '。' | '!' | '?' | '\n')
            || (character == '.' && next.is_none_or(char::is_whitespace));
        if !boundary {
            current.push(character);
        }
        if boundary || next.is_none() {
            let trimmed = current.trim_end_matches(|c: char| c.is_whitespace() || c == '」');
            let content = trimmed
                .chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<Vec<_>>();
            if !content.is_empty() {
                sentences.push(Sentence {
                    question: character == '?'
                        || JA_QUESTION_ENDINGS
                            .iter()
                            .any(|ending| trimmed.ends_with(ending)),
                    content,
                });
            }
            current.clear();
        }
    }
    sentences
}

fn similarity(left: &[char], right: &[char]) -> f64 {
    let grams = |text: &[char]| {
        if text.len() < 2 {
            return vec![text.to_vec()];
        }
        text.windows(2).map(<[char]>::to_vec).collect::<Vec<_>>()
    };
    let (left, mut right) = (grams(left), grams(right));
    let total = left.len() + right.len();
    let mut common = 0;
    for gram in &left {
        if let Some(position) = right.iter().position(|other| other == gram) {
            right.swap_remove(position);
            common += 1;
        }
    }
    (2 * common) as f64 / total.max(1) as f64
}

#[cfg(test)]
fn question_count(text: &str) -> usize {
    sentences(text)
        .iter()
        .filter(|sentence| sentence.question)
        .count()
}

/// Every spoken question stays a question. Each source question is matched
/// to the most similar output sentence, which must be a question
/// (確認できますか。送信できます。 -> 確認できます。送信できますか。 fails).
/// The output must also keep as many question sentences; with repetition
/// removal on, identical repeated questions count once.
pub(super) fn question_preserved(source: &str, output: &str, edits: FactEdits) -> bool {
    let source = sentences(source);
    let output = sentences(output);
    let mut questions = source
        .iter()
        .filter(|sentence| sentence.question)
        .map(|sentence| &sentence.content)
        .collect::<Vec<_>>();
    if questions.is_empty() {
        return true;
    }
    let aligned = questions.iter().all(|question| {
        output
            .iter()
            .map(|sentence| (similarity(question, &sentence.content), sentence.question))
            .filter(|(score, _)| *score > 0.0)
            .max_by(|left, right| left.0.total_cmp(&right.0))
            .is_some_and(|(_, is_question)| is_question)
    });
    if edits.merge_duplicates {
        questions.sort_unstable();
        questions.dedup();
    }
    aligned && output.iter().filter(|sentence| sentence.question).count() >= questions.len()
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

    fn q(source: &str, output: &str) -> bool {
        question_preserved(source, output, NONE)
    }

    #[test]
    fn a_repair_must_restate_the_negated_predicate() {
        // The cue repairs あした, not 送らないで.
        assert!(!negation_preserved(
            "本番に送らないであした、いやあさって確認して",
            "本番に送ってあさって確認して",
            ALL
        ));
        assert!(negation_preserved(
            "本番に送らないであした、いやあさって確認して",
            "本番に送らないであさって確認して",
            ALL
        ));
        // Without a kanji anchor (しない) the negation cannot be repaired away.
        assert!(!negation_preserved("しない、いやする", "する", ALL));
        assert!(!negation_preserved(
            "don't merge it, actually deploy it",
            "Deploy it.",
            ALL
        ));
    }

    #[test]
    fn each_question_must_map_to_an_interrogative_sentence() {
        assert!(!q(
            "確認できますか。送信できます。",
            "確認できます。送信できますか。"
        ));
        assert!(q(
            "確認できますか。送信できます。",
            "確認できますか？送信できます。"
        ));
        // An ASCII period ends a sentence again.
        assert_eq!(question_count("確認できますか. 明日です。"), 1);
        assert!(!q("確認できますか. 明日です。", "確認できます. 明日です。"));
        assert_eq!(question_count("Python 3.12 です。example.com ですか"), 1);
    }

    #[test]
    fn identical_repeated_questions_collapse_only_with_repetition_removal() {
        let merge = FactEdits {
            corrections: false,
            merge_duplicates: true,
        };
        let source = "確認できますか。確認できますか。";
        assert!(question_preserved(source, "確認できますか。", merge));
        assert!(!question_preserved(source, "確認できますか。", NONE));
        assert!(!question_preserved(
            "確認できますか。送信できますか。",
            "確認できますか。",
            merge
        ));
    }

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
        assert!(!q(
            "確認できますか。送信できますか。",
            "確認できますか。送信できます。"
        ));
        assert!(!q(
            "Can you check? Can you send it?",
            "Can you check? I can send it."
        ));
        assert!(q(
            "確認できますか。送信できますか。",
            "確認できますか？送信できますか？"
        ));
        assert_eq!(question_count("必要ですかどうか確認します。"), 0);
        assert_eq!(question_count("いいですか？ 明日来ますか"), 2);
    }

    #[test]
    fn questions_must_stay_questions() {
        assert!(!q(
            "この関数ってスレッドセーフですか？",
            "はい、この関数はスレッドセーフです。"
        ));
        assert!(!q("Is it thread safe?", "Yes, it is thread safe."));
        assert!(!q("明日来ますか", "明日来ます。"));
        assert!(q("明日来ますか", "明日来ますか？"));
        assert!(q(
            "この関数ってスレッドセーフですか？",
            "この関数はスレッドセーフですか？"
        ));
        assert!(q("can you tell me the deadline", "The deadline is Friday."));
        assert!(q("送ります。", "送ります。"));
        // ですか inside a clause is not a sentence-final question.
        assert!(q("必要ですかどうか確認します。", "必要か確認します。"));
    }
}
