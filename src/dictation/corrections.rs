//! Labs › Learn from corrections: what a user's edit to a dictation's text
//! teaches the vocabulary. Everything here is pure. [`watch`] follows the
//! field on macOS and hands the inserted and edited text to [`learn`].
//!
//! Only words the user typed in are learned, and only ones shaped like
//! names: capitalized mid-sentence, all capitals, mixed case, letters with
//! digits, or a short run of those. Plain lowercase words, words already in
//! the dictation, and terms already in the vocabulary are never learned.

#[cfg(target_os = "macos")]
pub(crate) mod watch;

use crate::dictation::vocabulary::{self, VocabularyEntry};

/// Longest dictation or edited text compared, in words.
const MAX_WORDS: usize = 400;
/// Above this share of words changed, the edit is a rewrite, not a
/// correction, and nothing is learned.
const MAX_DISTANCE: f32 = 0.6;
/// Most words in one learned phrase.
const MAX_PHRASE_WORDS: usize = 3;
/// Most terms one correction adds.
const MAX_TERMS: usize = 5;

/// What one edit to a dictation's text teaches.
#[derive(Debug, PartialEq)]
pub(crate) enum Outcome {
    /// The text reads the same once trimmed.
    Unchanged,
    /// The dictation or the edited text is over [`MAX_WORDS`] words.
    TooLong,
    /// Too much changed to be a correction.
    Rewrite,
    /// The terms to add, in the order they appear; empty when the edit
    /// introduced nothing name-like or nothing new.
    Learned(Vec<String>),
}

/// The terms `edited` introduced over `inserted` that `vocabulary` lacks.
pub(crate) fn learn(inserted: &str, edited: &str, vocabulary: &[VocabularyEntry]) -> Outcome {
    if inserted.trim() == edited.trim() {
        return Outcome::Unchanged;
    }
    let before = words(inserted);
    let after = words(edited);
    if before.len() > MAX_WORDS || after.len() > MAX_WORDS {
        return Outcome::TooLong;
    }
    let diff = diff(&before, &after);
    if diff.distance > MAX_DISTANCE {
        return Outcome::Rewrite;
    }
    let mut terms: Vec<String> = Vec::new();
    for term in diff
        .added
        .iter()
        .flat_map(|run| candidates(&after, run.clone(), &before))
    {
        if terms.len() == MAX_TERMS {
            break;
        }
        if term.chars().count() <= vocabulary::MAX_TERM_CHARS
            && !vocabulary::contains(vocabulary, &term)
            && !terms.iter().any(|kept| vocabulary::same(kept, &term))
        {
            terms.push(term);
        }
    }
    Outcome::Learned(terms)
}

/// Where a dictation landed in a field: the field's text before and after
/// it, as it read once the paste arrived.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Anchor {
    before: String,
    after: String,
}

/// The text around the last place `inserted` appears in `field`, or `None`
/// when it isn't there (yet).
pub(crate) fn anchor(field: &str, inserted: &str) -> Option<Anchor> {
    let inserted = inserted.trim();
    if inserted.is_empty() {
        return None;
    }
    let start = field.rfind(inserted)?;
    Some(Anchor {
        before: field[..start].to_owned(),
        after: field[start + inserted.len()..].to_owned(),
    })
}

/// What now stands where the dictation was: `field` less the text that
/// surrounded the dictation. `None` when the user changed that surrounding
/// text too, since an edit outside the dictation is no correction of it.
pub(crate) fn edited_region<'a>(anchor: &Anchor, field: &'a str) -> Option<&'a str> {
    let rest = field.strip_prefix(anchor.before.as_str())?;
    rest.strip_suffix(anchor.after.as_str())
}

/// One word as typed, and its letters without surrounding punctuation.
#[derive(Debug)]
struct Word<'a> {
    raw: &'a str,
    bare: &'a str,
}

fn words(text: &str) -> Vec<Word<'_>> {
    text.split_whitespace()
        .map(|raw| Word {
            raw,
            bare: raw
                .trim_start_matches(|c: char| !c.is_alphanumeric())
                .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '+' && c != '#'),
        })
        .filter(|word| !word.bare.is_empty())
        .collect()
}

/// A word-level comparison: the runs of edited words that stand where the
/// dictation had other words or none, as index ranges into the edited
/// words, and the share of words changed.
struct Diff {
    added: Vec<std::ops::Range<usize>>,
    distance: f32,
}

/// Levenshtein over words, then a walk back through the table to find
/// which edited words were inserted or substituted.
fn diff(before: &[Word], after: &[Word]) -> Diff {
    let (rows, columns) = (before.len() + 1, after.len() + 1);
    let mut cost = vec![0_usize; rows * columns];
    for i in 0..rows {
        cost[i * columns] = i;
    }
    for (j, cell) in cost.iter_mut().enumerate().take(columns) {
        *cell = j;
    }
    for i in 1..rows {
        for j in 1..columns {
            let substitute = usize::from(before[i - 1].bare != after[j - 1].bare);
            cost[i * columns + j] = (cost[(i - 1) * columns + j - 1] + substitute)
                .min(cost[(i - 1) * columns + j] + 1)
                .min(cost[i * columns + j - 1] + 1);
        }
    }
    let mut changed = vec![false; after.len()];
    let (mut i, mut j) = (before.len(), after.len());
    while i > 0 || j > 0 {
        let here = cost[i * columns + j];
        if i > 0 && j > 0 {
            let substitute = usize::from(before[i - 1].bare != after[j - 1].bare);
            if here == cost[(i - 1) * columns + j - 1] + substitute {
                changed[j - 1] = substitute == 1;
                i -= 1;
                j -= 1;
                continue;
            }
        }
        if j > 0 && here == cost[i * columns + j - 1] + 1 {
            changed[j - 1] = true;
            j -= 1;
        } else {
            i -= 1;
        }
    }
    let mut added = Vec::new();
    let mut start = None;
    for (index, &is_changed) in changed.iter().chain([&false]).enumerate() {
        match (is_changed, start) {
            (true, None) => start = Some(index),
            (false, Some(from)) => {
                added.push(from..index);
                start = None;
            }
            _ => {}
        }
    }
    let longest = before.len().max(after.len()).max(1);
    Diff {
        added,
        distance: cost[rows * columns - 1] as f32 / longest as f32,
    }
}

/// The terms in one run of edited words: each stretch of up to
/// [`MAX_PHRASE_WORDS`] adjacent name-like words, unbroken by punctuation,
/// that the dictation didn't already have. A capital that only starts a
/// sentence doesn't make a name.
fn candidates(after: &[Word], run: std::ops::Range<usize>, before: &[Word]) -> Vec<String> {
    let mut terms = Vec::new();
    let mut phrase: Vec<&str> = Vec::new();
    let mut flush = |phrase: &mut Vec<&str>| {
        if !phrase.is_empty() {
            terms.push(phrase.join(" "));
            phrase.clear();
        }
    };
    for index in run {
        let word = &after[index];
        let starts_sentence = index == 0 || ends_sentence(after[index - 1].raw);
        let typed_in = !before.iter().any(|kept| kept.bare == word.bare);
        let name = typed_in
            && looks_like_name(word.bare)
            && !(starts_sentence && is_capitalized(word.bare));
        if !name || phrase.len() == MAX_PHRASE_WORDS || !word.raw.starts_with(word.bare) {
            flush(&mut phrase);
        }
        if name {
            phrase.push(word.bare);
            if !word.raw.ends_with(word.bare) {
                flush(&mut phrase);
            }
        }
    }
    flush(&mut phrase);
    terms
}

fn ends_sentence(raw: &str) -> bool {
    raw.trim_end_matches(['"', '\'', ')', ']', '”', '’'])
        .ends_with(['.', '!', '?'])
}

/// A word shaped like a name: capitalized ("Raunak"), all capitals
/// ("NASA"), mixed case ("iPhone", "OpenAI"), or letters with digits
/// ("GPT4"). Single letters and plain lowercase words are not.
fn looks_like_name(word: &str) -> bool {
    let letters = word.chars().filter(|c| c.is_alphabetic()).count();
    if letters == 0 || word.chars().count() < 2 {
        return false;
    }
    let digits = word.chars().any(|c| c.is_ascii_digit());
    let uppercase = word.chars().filter(|c| c.is_uppercase()).count();
    digits || uppercase > 0
}

/// An initial capital and nothing else that marks a name: the shape every
/// sentence's first word has.
fn is_capitalized(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next().is_some_and(char::is_uppercase)
        && !chars.any(|c| c.is_uppercase() || c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(term: &str) -> VocabularyEntry {
        VocabularyEntry {
            term: term.into(),
            sounds_like: Vec::new(),
        }
    }

    fn learned(inserted: &str, edited: &str) -> Vec<String> {
        match learn(inserted, edited, &[]) {
            Outcome::Learned(terms) => terms,
            other => panic!("expected terms, got {other:?}"),
        }
    }

    #[test]
    fn unchanged_text_teaches_nothing() {
        assert_eq!(
            learn("Send it to Ronak.", "  Send it to Ronak. ", &[]),
            Outcome::Unchanged
        );
    }

    #[test]
    fn one_corrected_name_is_learned() {
        assert_eq!(
            learned(
                "can you send the deck to ronak by friday",
                "can you send the deck to Raunak by friday"
            ),
            ["Raunak"]
        );
    }

    #[test]
    fn a_capitalization_fix_counts_as_a_correction() {
        assert_eq!(
            learned("ask open ai about it", "ask OpenAI about it"),
            ["OpenAI"]
        );
    }

    #[test]
    fn a_corrected_full_name_is_learned_as_one_term() {
        assert_eq!(
            learned(
                "meeting with ronak chowdry tomorrow.",
                "meeting with Raunak Chowdhuri tomorrow."
            ),
            ["Raunak Chowdhuri"]
        );
    }

    #[test]
    fn punctuation_splits_names_into_separate_terms() {
        assert_eq!(
            learned("invite ron and sam", "invite Raunak, Sahil and sam"),
            ["Raunak", "Sahil"]
        );
    }

    #[test]
    fn all_caps_and_digits_are_learned_but_plain_words_are_not() {
        assert_eq!(
            learned(
                "we said the gpt four model ships soon in q three for sure",
                "we said the GPT-4o model ships very soon in Q3 for sure"
            ),
            ["GPT-4o", "Q3"]
        );
    }

    #[test]
    fn a_capital_that_only_starts_a_sentence_is_not_a_name() {
        assert_eq!(
            learned("ok. the build passed", "ok. Then the build passed"),
            Vec::<String>::new()
        );
        assert_eq!(
            learned("ok. ronak chowdry said so", "ok. Raunak Chowdhuri said so"),
            ["Chowdhuri"]
        );
    }

    #[test]
    fn words_the_dictation_already_had_are_not_learned() {
        assert_eq!(
            learned(
                "tell Anna that Paris is lovely",
                "tell Paris that Anna is lovely"
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn deleting_words_teaches_nothing() {
        assert_eq!(
            learned("tell Raunak hi and bye", "tell Raunak hi"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_wholesale_rewrite_is_not_a_correction() {
        assert_eq!(
            learn(
                "so I was thinking we could maybe meet up",
                "Lunch with Priya at Zuni Cafe on Thursday",
                &[]
            ),
            Outcome::Rewrite
        );
    }

    #[test]
    fn text_over_the_word_limit_is_skipped() {
        let long = "word ".repeat(MAX_WORDS + 1);
        let edited = format!("{long} Raunak");
        assert_eq!(learn(&long, &edited, &[]), Outcome::TooLong);
        assert_eq!(learn("hi ronak", &edited, &[]), Outcome::TooLong);
    }

    #[test]
    fn terms_already_in_the_vocabulary_are_skipped_in_any_case() {
        let vocabulary = [entry("raunak"), entry("Hex")];
        assert_eq!(
            learn(
                "ask ronak about hecks and zed",
                "ask Raunak about Hex and Zed",
                &vocabulary
            ),
            Outcome::Learned(vec!["Zed".into()])
        );
    }

    #[test]
    fn a_term_typed_twice_is_learned_once() {
        assert_eq!(
            learned(
                "ronak met ronak at the office today",
                "Raunak met Raunak at the office today"
            ),
            ["Raunak"]
        );
    }

    #[test]
    fn the_edited_region_is_what_replaced_the_dictation() {
        let field = "Hi team,\nsend it to ronak today\nThanks";
        let anchor = anchor(field, " send it to ronak today ").expect("the dictation is there");
        let edited = "Hi team,\nsend it to Raunak today\nThanks";
        assert_eq!(
            edited_region(&anchor, edited),
            Some("send it to Raunak today")
        );
    }

    #[test]
    fn an_edit_outside_the_dictation_has_no_region() {
        let field = "Hi team, send it to ronak";
        let anchor = anchor(field, "send it to ronak").expect("the dictation is there");
        assert_eq!(edited_region(&anchor, "Hello all, send it to Raunak"), None);
        assert_eq!(anchor_missing(), None);
    }

    fn anchor_missing() -> Option<Anchor> {
        anchor("nothing here", "send it")
    }
}
