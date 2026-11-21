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
    /// The user changed the text around the dictation too.
    Outside,
    /// The dictation or the edited text is over [`MAX_WORDS`] words.
    TooLong,
    /// Too much changed to be a correction.
    Rewrite,
    /// The terms to add, in the order they appear; empty when the edit
    /// introduced nothing name-like or nothing new.
    Learned(Vec<String>),
}

/// Which sides of a dictation had no text next to it when it landed. Words
/// typed on past an open side are the user drafting on, not correcting.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Open {
    pub(crate) start: bool,
    pub(crate) end: bool,
}

/// The terms `edited` introduced over `inserted` that `vocabulary` lacks.
pub(crate) fn learn(
    inserted: &str,
    edited: &str,
    open: Open,
    vocabulary: &[VocabularyEntry],
) -> Outcome {
    if inserted.trim() == edited.trim() {
        return Outcome::Unchanged;
    }
    let before = words(inserted);
    let after = words(edited);
    if before.len() > MAX_WORDS || after.len() > MAX_WORDS {
        return Outcome::TooLong;
    }
    let diff = diff(&before, &after, open);
    if diff.distance > MAX_DISTANCE {
        return Outcome::Rewrite;
    }
    let mut terms: Vec<String> = Vec::new();
    for term in diff
        .added
        .iter()
        .flat_map(|run| candidates(&after, run.clone(), &before))
    {
        if term.chars().count() <= vocabulary::MAX_TERM_CHARS
            && !vocabulary::contains(vocabulary, &term)
            && !terms.iter().any(|kept| vocabulary::same(kept, &term))
        {
            terms.push(term);
        }
    }
    Outcome::Learned(terms)
}

/// The entries one correction added, for its Undo, and the whole
/// vocabulary as that left it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Learned {
    added: Vec<VocabularyEntry>,
    saved: Vec<VocabularyEntry>,
}

impl Learned {
    pub(crate) fn is_empty(&self) -> bool {
        self.added.is_empty()
    }

    pub(crate) fn terms(&self) -> impl ExactSizeIterator<Item = &str> {
        self.added.iter().map(|entry| entry.term.as_str())
    }

    /// Takes these entries back out of `vocabulary` if it is still exactly
    /// as this correction left it; any change since, even a term removed
    /// and added again, means the user has taken it over, and nothing is
    /// removed. Returns how many it removed.
    pub(crate) fn undo(&self, vocabulary: &mut Vec<VocabularyEntry>) -> usize {
        if *vocabulary != self.saved {
            return 0;
        }
        vocabulary.retain(|entry| !self.added.contains(entry));
        self.added.len()
    }
}

/// Adds up to [`MAX_TERMS`] of the learned `terms` to `vocabulary`,
/// skipping any it has in any case and any past its 500-entry cap, so a
/// term the user added meanwhile doesn't take a learned one's place.
/// Returns what it added.
pub(crate) fn add_learned(vocabulary: &mut Vec<VocabularyEntry>, terms: &[String]) -> Learned {
    let added = terms
        .iter()
        .filter_map(|term| {
            (vocabulary::add(vocabulary, term, &[]) == Ok(true))
                .then(|| vocabulary.last().cloned())
                .flatten()
        })
        .take(MAX_TERMS)
        .collect();
    Learned {
        added,
        saved: vocabulary.clone(),
    }
}

/// Where a dictation landed in a field: the field's text before and after
/// it, as it read once the paste arrived.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Anchor {
    before: String,
    after: String,
}

impl Anchor {
    /// The sides with only whitespace next to the dictation.
    pub(crate) fn open(&self) -> Open {
        Open {
            start: self.before.trim().is_empty(),
            end: self.after.trim().is_empty(),
        }
    }
}

/// The text around `inserted` in `field`. With `caret`, the insertion point
/// in UTF-16 units as the paste left it, `inserted` must end right there;
/// without one it must appear exactly once, since a guess could compare the
/// wrong copy. `None` when it isn't there (yet), or the field can't say
/// which copy is the dictation.
pub(crate) fn anchor(field: &str, inserted: &str, caret: Option<usize>) -> Option<Anchor> {
    let trimmed = inserted.trim();
    if trimmed.is_empty() {
        return None;
    }
    let start = match caret {
        Some(caret) => {
            let end = byte_offset(field, caret)?;
            let start = end.checked_sub(inserted.len())?;
            if field.get(start..end)? != inserted {
                return None;
            }
            start + (inserted.len() - inserted.trim_start().len())
        }
        None => {
            let mut found = field.match_indices(trimmed).map(|(start, _)| start);
            let start = found.next()?;
            if found.next().is_some() {
                return None;
            }
            start
        }
    };
    Some(Anchor {
        before: field[..start].to_owned(),
        after: field[start + trimmed.len()..].to_owned(),
    })
}

/// The byte offset of `units` UTF-16 code units into `text`, or `None` past
/// its end or inside a character.
fn byte_offset(text: &str, units: usize) -> Option<usize> {
    let mut seen = 0;
    for (offset, c) in text.char_indices() {
        if seen == units {
            return Some(offset);
        }
        if seen > units {
            return None;
        }
        seen += c.len_utf16();
    }
    (seen == units).then_some(text.len())
}

/// What now stands where the dictation was: `field` less the text that
/// surrounded the dictation. `None` when the user changed that surrounding
/// text too, since an edit outside the dictation is no correction of it.
pub(crate) fn edited_region<'a>(anchor: &Anchor, field: &'a str) -> Option<&'a str> {
    let rest = field.strip_prefix(anchor.before.as_str())?;
    rest.strip_suffix(anchor.after.as_str())
}

/// One word as typed, its letters without surrounding punctuation, and
/// whether punctuation standing on its own (" – ") came just before it.
#[derive(Debug)]
struct Word<'a> {
    raw: &'a str,
    bare: &'a str,
    after_mark: bool,
}

fn words(text: &str) -> Vec<Word<'_>> {
    let mut words = Vec::new();
    let mut mark = false;
    for raw in text.split_whitespace() {
        let bare = raw
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '+' && c != '#');
        if bare.is_empty() {
            mark = true;
            continue;
        }
        words.push(Word {
            raw,
            bare,
            after_mark: std::mem::take(&mut mark),
        });
    }
    words
}

/// A word-level comparison: the runs of edited words that stand where the
/// dictation had other words or none, as index ranges into the edited
/// words, and the share of words changed.
struct Diff {
    added: Vec<std::ops::Range<usize>>,
    distance: f32,
}

/// Levenshtein over words, then a walk back through the table to find
/// which edited words were inserted or substituted. Edited words before an
/// open start or past an open end cost nothing and never count as changed.
fn diff(before: &[Word], after: &[Word], open: Open) -> Diff {
    let (rows, columns) = (before.len() + 1, after.len() + 1);
    let mut cost = vec![0_usize; rows * columns];
    for i in 0..rows {
        cost[i * columns] = i;
    }
    for (j, cell) in cost.iter_mut().enumerate().take(columns) {
        *cell = if open.start { 0 } else { j };
    }
    for i in 1..rows {
        for j in 1..columns {
            let substitute = usize::from(before[i - 1].bare != after[j - 1].bare);
            cost[i * columns + j] = (cost[(i - 1) * columns + j - 1] + substitute)
                .min(cost[(i - 1) * columns + j] + 1)
                .min(cost[i * columns + j - 1] + 1);
        }
    }
    let last = before.len() * columns;
    // Past an open end, the cheapest place for the dictation to stop, the
    // later one on a tie.
    let end = if open.end {
        (0..columns)
            .rev()
            .min_by_key(|&j| cost[last + j])
            .unwrap_or(0)
    } else {
        after.len()
    };
    let mut changed = vec![false; after.len()];
    let mut replaced = vec![false; after.len()];
    let (mut i, mut j) = (before.len(), end);
    while i > 0 || (j > 0 && !open.start) {
        let here = cost[i * columns + j];
        if i > 0 && j > 0 {
            let substitute = usize::from(before[i - 1].bare != after[j - 1].bare);
            if here == cost[(i - 1) * columns + j - 1] + substitute {
                changed[j - 1] = substitute == 1;
                replaced[j - 1] = substitute == 1;
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
    // A name typed over dictated words can take more words than it replaced
    // ("ronak" -> "Raunak Chowdhuri"): up to a phrase's length, the extra
    // words are the name, not more edits. Past an open end, a name typed
    // over the last word runs on past where the dictation stopped. Any
    // name-shaped word starts the run, but only words that can be learned
    // take a phrase's places: a sentence's first word or one the dictation
    // had ("Thanks. Raunak Chowdhuri") brings its surname along without
    // using one up.
    let mut forgiven = 0;
    let mut phrase = NamePhrase::default();
    for index in 0..after.len() {
        let continues = phrase.replaced + phrase.inserted > 0
            && phrase.words < MAX_PHRASE_WORDS
            && joined(&after[index - 1], &after[index])
            && looks_like_name(after[index].bare);
        if index >= end {
            if !(open.end && continues && phrase.replaced > 0) {
                break;
            }
            changed[index] = true;
            phrase.words += 1;
        } else if changed[index] && looks_like_name(after[index].bare) {
            if !continues {
                forgiven += phrase.forgiven();
                phrase = NamePhrase::default();
            }
            phrase.words += usize::from(is_name(after, index, before));
            if replaced[index] {
                phrase.replaced += 1;
            } else {
                phrase.inserted += 1;
            }
        } else {
            forgiven += phrase.forgiven();
            phrase = NamePhrase::default();
        }
    }
    forgiven += phrase.forgiven();
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
    // Neither a name's run past an open end nor the words it took on count
    // as words the edit could have changed, or a rewrite ending in a long
    // name would pass for a correction.
    let longest = before.len().max(end - j - forgiven).max(1);
    Diff {
        added,
        distance: (cost[last + end] - forgiven) as f32 / longest as f32,
    }
}

/// Adjacent changed name-shaped words in the edited text: how many of them
/// can be learned, and how many replaced a dictated word or were inserted.
#[derive(Default)]
struct NamePhrase {
    words: usize,
    replaced: usize,
    inserted: usize,
}

impl NamePhrase {
    /// The inserted words a name that replaced dictated words took on.
    fn forgiven(&self) -> usize {
        if self.replaced > 0 { self.inserted } else { 0 }
    }
}

/// Whether `next` follows `word` in one phrase, no punctuation between.
fn joined(word: &Word, next: &Word) -> bool {
    word.raw.ends_with(word.bare) && !next.after_mark && next.raw.starts_with(next.bare)
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
        let name = is_name(after, index, before);
        if !name
            || phrase.len() == MAX_PHRASE_WORDS
            || word.after_mark
            || !word.raw.starts_with(word.bare)
        {
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

/// Whether edited word `index` can be part of a learned term: typed in,
/// shaped like a name, and not just a sentence's first word.
fn is_name(after: &[Word], index: usize, before: &[Word]) -> bool {
    let word = &after[index];
    let starts_sentence = index == 0 || ends_sentence(after[index - 1].raw);
    !before.iter().any(|kept| kept.bare == word.bare)
        && looks_like_name(word.bare)
        && !(starts_sentence && is_capitalized(word.bare))
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
        match learn(inserted, edited, Open::default(), &[]) {
            Outcome::Learned(terms) => terms,
            other => panic!("expected terms, got {other:?}"),
        }
    }

    #[test]
    fn unchanged_text_teaches_nothing() {
        assert_eq!(
            learn(
                "Send it to Ronak.",
                "  Send it to Ronak. ",
                Open::default(),
                &[]
            ),
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
                Open::default(),
                &[]
            ),
            Outcome::Rewrite
        );
    }

    #[test]
    fn text_over_the_word_limit_is_skipped() {
        let long = "word ".repeat(MAX_WORDS + 1);
        let edited = format!("{long} Raunak");
        assert_eq!(
            learn(&long, &edited, Open::default(), &[]),
            Outcome::TooLong
        );
        assert_eq!(
            learn("hi ronak", &edited, Open::default(), &[]),
            Outcome::TooLong
        );
    }

    #[test]
    fn terms_already_in_the_vocabulary_are_skipped_in_any_case() {
        let vocabulary = [entry("raunak"), entry("Hex")];
        assert_eq!(
            learn(
                "ask ronak about hecks and zed",
                "ask Raunak about Hex and Zed",
                Open::default(),
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
        let anchor =
            anchor(field, " send it to ronak today ", None).expect("the dictation is there");
        let edited = "Hi team,\nsend it to Raunak today\nThanks";
        assert_eq!(
            edited_region(&anchor, edited),
            Some("send it to Raunak today")
        );
    }

    #[test]
    fn an_edit_outside_the_dictation_has_no_region() {
        let field = "Hi team, send it to ronak";
        let anchor = anchor(field, "send it to ronak", None).expect("the dictation is there");
        assert_eq!(edited_region(&anchor, "Hello all, send it to Raunak"), None);
        assert_eq!(anchor_missing(), None);
    }

    fn anchor_missing() -> Option<Anchor> {
        anchor("nothing here", "send it", None)
    }

    /// What `field_now` teaches about `inserted`, which the field read as
    /// `field_then` once it landed and where it appears once.
    fn learned_in_field(field_then: &str, inserted: &str, field_now: &str) -> Vec<String> {
        let anchor = anchor(field_then, inserted, None).expect("the dictation is there");
        let edited = edited_region(&anchor, field_now).expect("only the dictation changed");
        match learn(inserted, edited, anchor.open(), &[]) {
            Outcome::Learned(terms) => terms,
            other => panic!("expected terms, got {other:?}"),
        }
    }

    #[test]
    fn spaced_punctuation_splits_names_into_separate_terms() {
        assert_eq!(
            learned(
                "invite ron and sam today please",
                "invite Raunak – Sahil today please"
            ),
            ["Raunak", "Sahil"]
        );
    }

    #[test]
    fn text_typed_on_after_a_paste_at_the_end_is_not_a_correction() {
        let inserted = "Please send the deck to ronak today.";
        let field = format!("Hi team, {inserted}");
        assert_eq!(
            learned_in_field(
                &field,
                inserted,
                &format!("{field} I spoke with Priya this morning.")
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            learned_in_field(inserted, inserted, &format!("Priya said: {inserted}")),
            Vec::<String>::new(),
            "nor is text typed in before a paste into an empty field"
        );
    }

    #[test]
    fn a_corrected_last_word_at_the_end_of_the_field_is_learned() {
        let field = "Hi team, send the deck to ronak";
        assert_eq!(
            learned_in_field(
                field,
                "send the deck to ronak",
                "Hi team, send the deck to Raunak"
            ),
            ["Raunak"]
        );
        assert_eq!(
            learned_in_field(
                field,
                "send the deck to ronak",
                "Hi team, send the deck to Raunak. I spoke with Priya."
            ),
            ["Raunak"],
            "the correction counts, the sentence typed after it doesn't"
        );
        assert_eq!(
            learned_in_field(
                field,
                "send the deck to ronak",
                "Hi team, send the deck to Raunak Chowdhuri"
            ),
            ["Raunak Chowdhuri"],
            "a full name typed over the last word"
        );
        assert_eq!(
            learned_in_field(
                field,
                "send the deck to ronak",
                "Hi team, send the deck to Raunak Chowdhuri. Priya agreed."
            ),
            ["Raunak Chowdhuri"],
            "and drafting typed after that full name"
        );
    }

    #[test]
    fn a_name_runs_on_only_from_a_name_typed_over_the_last_word() {
        let field = "Hi team, send this to ryan";
        assert_eq!(
            learned_in_field(
                field,
                "send this to ryan",
                "Hi team, send this to him Monday"
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_name_running_on_stops_at_a_phrase_counting_its_corrected_words() {
        let field = "Notes: send the deck to jean luc";
        assert_eq!(
            learned_in_field(
                field,
                "send the deck to jean luc",
                "Notes: send the deck to Jean Luc Picard Monday"
            ),
            ["Jean Luc Picard"]
        );
    }

    #[test]
    fn a_longer_name_typed_over_a_short_dictation_is_a_correction() {
        assert_eq!(
            learned("ask ronak", "ask Raunak Chowdhuri"),
            ["Raunak Chowdhuri"]
        );
        assert_eq!(
            learned_in_field(
                "Hi team, ask ronak",
                "ask ronak",
                "Hi team, ask Raunak Chowdhuri"
            ),
            ["Raunak Chowdhuri"],
            "and at the end of the field"
        );
    }

    #[test]
    fn a_rewrite_ending_in_a_long_name_is_not_a_correction() {
        let field = "Hi team, call me tomorrow";
        let inserted = "call me tomorrow";
        let anchor = anchor(field, inserted, None).expect("the dictation is there");
        let edited = edited_region(&anchor, "Hi team, ask for Alice Smith Monday")
            .expect("only the dictation changed");
        assert_eq!(
            learn(inserted, edited, anchor.open(), &[]),
            Outcome::Rewrite
        );
    }

    #[test]
    fn a_sentence_starter_takes_no_place_in_a_name() {
        assert_eq!(
            learned_in_field(
                "Notes: I booked the room. Tell john",
                "I booked the room. Tell john",
                "Notes: I booked the room. Ask Jean Luc Picard"
            ),
            ["Jean Luc Picard"]
        );
    }

    #[test]
    fn a_name_that_cannot_be_learned_still_brings_its_surname() {
        assert_eq!(
            learned_in_field(
                "Thanks for your help. ronak",
                "Thanks for your help. ronak",
                "Thanks for your help. Raunak Chowdhuri"
            ),
            ["Chowdhuri"],
            "a sentence's first word"
        );
        assert_eq!(
            learned_in_field(
                "Notes: John called jon",
                "John called jon",
                "Notes: John called John Smythe"
            ),
            ["Smythe"],
            "a word the dictation had"
        );
    }

    #[test]
    fn the_caret_picks_the_dictation_over_an_identical_later_copy() {
        let inserted = "send the deck to ronak today.";
        let later = " Again: send the deck to ronak today.";
        let field = format!("Notes: {inserted}{later}");
        let caret = format!("Notes: {inserted}").encode_utf16().count();
        let anchor =
            anchor(&field, inserted, Some(caret)).expect("the dictation ends at the caret");
        let edited = format!("Notes: send the deck to Raunak today.{later}");
        assert_eq!(
            edited_region(&anchor, &edited),
            Some("send the deck to Raunak today.")
        );
    }

    #[test]
    fn without_a_caret_a_repeated_dictation_is_not_guessed() {
        let field = "send it. send it.";
        assert_eq!(anchor(field, "send it.", None), None);
        assert_eq!(
            anchor(field, "send it.", Some(3)),
            None,
            "a caret the dictation doesn't end at"
        );
    }

    #[test]
    fn the_caret_counts_utf16_units_and_keeps_a_leading_space_out() {
        let field = "Café 👋 hi ronak";
        let caret = field.encode_utf16().count();
        let anchor =
            anchor(field, " hi ronak", Some(caret)).expect("the dictation ends at the caret");
        assert_eq!(
            edited_region(&anchor, "Café 👋 hi Raunak"),
            Some("hi Raunak")
        );
    }

    #[test]
    fn learned_terms_join_the_list_once_and_stop_at_its_cap() {
        let mut list = vec![entry("raunak")];
        let terms = ["Raunak".to_string(), "Zed".into(), "Zed".into()];
        assert_eq!(
            add_learned(&mut list, &terms).terms().collect::<Vec<_>>(),
            ["Zed"]
        );
        assert_eq!(list, [entry("raunak"), entry("Zed")]);
        let mut full: Vec<_> = (0..vocabulary::MAX_ENTRIES)
            .map(|index| entry(&format!("Term{index}")))
            .collect();
        assert!(add_learned(&mut full, &terms).is_empty());
        assert_eq!(full.len(), vocabulary::MAX_ENTRIES);
    }

    #[test]
    fn a_term_added_meanwhile_takes_no_learned_terms_place() {
        let inserted = "ping ann bob cat dan eve fay about the plan for the launch next week";
        let Outcome::Learned(terms) = learn(
            inserted,
            "ping Ann, Bob, Cat, Dan, Eve, Fay about the plan for the launch next week",
            Open::default(),
            &[],
        ) else {
            panic!("a correction");
        };
        // "Ann" was added in the Vocabulary pane while the watch ran.
        let mut list = vec![entry("Ann")];
        assert_eq!(
            add_learned(&mut list, &terms).terms().collect::<Vec<_>>(),
            ["Bob", "Cat", "Dan", "Eve", "Fay"]
        );
    }

    #[test]
    fn undo_takes_back_only_what_the_correction_added() {
        let mut list = vec![entry("OpenCode")];
        let learned = add_learned(&mut list, &["Raunak".to_string(), "Zed".into()]);
        assert_eq!(learned.terms().len(), 2);
        assert_eq!(learned.undo(&mut list), 2);
        assert_eq!(list, [entry("OpenCode")]);
        assert_eq!(learned.undo(&mut list), 0, "a second Undo changes nothing");
    }

    #[test]
    fn undo_leaves_a_term_edited_or_removed_since() {
        let mut list = vec![entry("OpenCode")];
        let learned = add_learned(
            &mut list,
            &["Raunak".to_string(), "Zed".into(), "Priya".into()],
        );
        // In the Vocabulary pane: a respelling, a "sounds like", a removal.
        list[1].term = "RAUNAK".into();
        list[2].sounds_like = vec!["said".into()];
        vocabulary::remove(&mut list, "Priya");
        let edited = list.clone();
        assert_eq!(learned.undo(&mut list), 0);
        assert_eq!(list, edited);
    }

    #[test]
    fn undo_leaves_a_term_removed_and_added_again() {
        let mut list = vec![entry("OpenCode")];
        let learned = add_learned(&mut list, &["Raunak".to_string(), "Zed".into()]);
        vocabulary::remove(&mut list, "Raunak");
        assert_eq!(vocabulary::add(&mut list, "Raunak", &[]), Ok(true));
        let edited = list.clone();
        assert_eq!(learned.undo(&mut list), 0, "the user's own Raunak stays");
        assert_eq!(list, edited, "and so does Zed, saved alongside it");
    }
}
