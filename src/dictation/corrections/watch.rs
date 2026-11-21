//! Follows the field a dictation landed in until the user is done with it,
//! then saves what [`super::learn`] finds to the vocabulary.
//!
//! The watch ends at Return, a focus change, a field that disappears or is
//! emptied, a newer insertion, or five minutes. It learns from the last read
//! of the field taken before any of those, so a message sent with Return or
//! a newer dictation pasted on top never counts as the user's edit. Nothing
//! here logs the text.

use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::{Outcome, anchor, edited_region, learn};
use crate::accessibility::WatchedField;
use crate::dictation::vocabulary;

const POLL: Duration = Duration::from_millis(100);
const LONGEST_WATCH: Duration = Duration::from_secs(5 * 60);
/// How long the pasted text has to show up in the field before the watch
/// gives up on it: a paste lands asynchronously, and some apps never expose
/// their text.
const ARRIVAL: Duration = Duration::from_secs(2);

/// Bumped as every insertion starts, so an older watch stops.
static INSERTIONS: AtomicU64 = AtomicU64::new(0);
/// Bumped on every Return or Enter key press the input monitor sees.
static RETURNS: AtomicU64 = AtomicU64::new(0);

/// One insertion, claimed before its paste is posted.
pub(crate) struct Insertion(u64);

/// Marks an insertion about to land; every earlier watch ends. Called
/// before the paste, so any read a watch confirms afterwards predates it.
pub(crate) fn begin() -> Insertion {
    Insertion(INSERTIONS.fetch_add(1, Ordering::AcqRel) + 1)
}

/// The input monitor saw Return or Enter go down.
pub(crate) fn note_key_down(code: u16) {
    const RETURN: u16 = 36;
    const KEYPAD_ENTER: u16 = 76;
    if matches!(code, RETURN | KEYPAD_ENTER) {
        RETURNS.fetch_add(1, Ordering::AcqRel);
    }
}

impl Insertion {
    /// Starts following the focused field of `pid` for `inserted`, when
    /// Labs › Learn from corrections is on.
    pub(crate) fn watch(self, pid: Option<i32>, inserted: &str) {
        if !crate::labs::learn_from_corrections() {
            return;
        }
        let Some(pid) = pid else {
            return;
        };
        let inserted = inserted.to_owned();
        let returns = RETURNS.load(Ordering::Acquire);
        let spawned = thread::Builder::new()
            .name("correction-watch".into())
            .spawn(move || follow(self.0, returns, pid, &inserted));
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start watching for corrections");
        }
    }
}

fn follow(insertion: u64, returns: u64, pid: i32, inserted: &str) {
    let current = || {
        INSERTIONS.load(Ordering::Acquire) == insertion
            && RETURNS.load(Ordering::Acquire) == returns
    };
    let Some(field) = WatchedField::focused(pid) else {
        return;
    };
    let started = Instant::now();
    let (anchor, mut last) = loop {
        if !current() || started.elapsed() > ARRIVAL {
            return;
        }
        if let Some(text) = field.text()
            && let Some(anchor) = anchor(&text, inserted)
        {
            break (anchor, text);
        }
        thread::sleep(POLL);
    };
    while started.elapsed() < LONGEST_WATCH && current() && field.has_focus() {
        thread::sleep(POLL);
        let Some(text) = field.text().filter(|text| !text.trim().is_empty()) else {
            break;
        };
        // Read before checking: a read that started after a newer paste or a
        // Return could already hold what they did to the field.
        if !current() {
            break;
        }
        last = text;
    }
    let Some(edited) = edited_region(&anchor, &last) else {
        tracing::debug!("correction watch: the text around the dictation changed");
        return;
    };
    match learn(inserted, edited, &vocabulary::entries()) {
        Outcome::Learned(terms) if !terms.is_empty() => match save(&terms) {
            Ok(added) => tracing::info!(added, "learned vocabulary from a correction"),
            Err(error) => tracing::warn!(%error, "could not save learned vocabulary"),
        },
        outcome => tracing::debug!(
            outcome = outcome_name(&outcome),
            "correction watch learned nothing"
        ),
    }
}

/// Adds `terms` to the saved vocabulary as `hex vocabulary add` would, then
/// has the app reload its settings. Existing entries are never touched.
fn save(terms: &[String]) -> color_eyre::Result<usize> {
    let mut settings = crate::app_settings::AppSettings::load()?;
    let base = settings.clone();
    let mut added = 0;
    for term in terms {
        if !vocabulary::contains(&settings.vocabulary, term)
            && vocabulary::add(&mut settings.vocabulary, term, &[]) == Ok(true)
        {
            added += 1;
        }
    }
    if added > 0 {
        base.update(&settings)?;
        if let Some(runtime) = crate::macos_rune::runtime() {
            let _ = runtime.send(crate::macos_rune::Request::ReloadSettings);
        }
    }
    Ok(added)
}

fn outcome_name(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Unchanged => "unchanged",
        Outcome::TooLong => "too-long",
        Outcome::Rewrite => "rewrite",
        Outcome::Learned(_) => "nothing-new",
    }
}
