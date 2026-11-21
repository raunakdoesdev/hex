//! Follows the field a dictation landed in until the user is done with it,
//! then saves what [`super::learn`] finds to the vocabulary.
//!
//! The watch ends at Return (typed, or pressed by YOLO mode), the app or the
//! field losing focus, a field that disappears or is emptied, a newer
//! insertion, the next dictation starting, the lab turning off, or five
//! minutes. It learns from the last read of the field taken before any of
//! those, so a message sent with Return or a newer dictation pasted on top
//! never counts as the user's edit, and a lab turned off saves nothing.
//! Nothing here logs the text.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::{Anchor, Outcome, add_learned, anchor, edited_region, learn};
use crate::accessibility::WatchedField;
use crate::app_settings::AppSettings;
use crate::dictation::vocabulary;

const POLL: Duration = Duration::from_millis(100);
const LONGEST_WATCH: Duration = Duration::from_secs(5 * 60);
/// How long the pasted text has to show up in the field before the watch
/// gives up on it: a paste lands asynchronously, and some apps never expose
/// their text.
const ARRIVAL: Duration = Duration::from_secs(2);

/// Bumped by everything that ends a watch: an insertion starting, Return,
/// and a dictation starting.
static EPOCH: AtomicU64 = AtomicU64::new(0);
/// What the running watch's last read teaches, for a dictation that starts
/// before the watch has saved it.
static PENDING: Mutex<Pending> = Mutex::new(Pending {
    epoch: 0,
    terms: Vec::new(),
});

struct Pending {
    epoch: u64,
    terms: Vec<String>,
}

/// One insertion, claimed before its paste is posted.
pub(crate) struct Insertion(u64);

fn end() -> u64 {
    EPOCH.fetch_add(1, Ordering::AcqRel) + 1
}

/// Marks an insertion about to land; every earlier watch ends. Called
/// before the paste, so any read a watch confirms afterwards predates it.
pub(crate) fn begin() -> Insertion {
    Insertion(end())
}

/// The input monitor saw Return or Enter go down.
pub(crate) fn note_key_down(code: u16) {
    const RETURN: u16 = 36;
    const KEYPAD_ENTER: u16 = 76;
    if matches!(code, RETURN | KEYPAD_ENTER) {
        end();
    }
}

/// Hex is about to press Return itself (YOLO mode), which the input monitor
/// skips as its own key.
pub(crate) fn note_return() {
    end();
}

/// A dictation is starting: the watch ends, and the terms its last read
/// already taught join the vocabulary in memory, so this capture hears
/// them. The watch saves them on its own thread; nothing here reads
/// Accessibility or settings.
pub(crate) fn dictation_started() {
    let terms = {
        let mut pending = PENDING.lock().unwrap_or_else(|error| error.into_inner());
        let running = pending.epoch == EPOCH.load(Ordering::Acquire);
        end();
        if running {
            std::mem::take(&mut pending.terms)
        } else {
            Vec::new()
        }
    };
    if !terms.is_empty() && crate::labs::learn_from_corrections() {
        vocabulary::add_unsaved(&terms);
    }
}

/// Holds `terms` for a dictation that starts next, unless the watch of
/// `epoch` already ended.
fn publish(epoch: u64, terms: &[String]) {
    let mut pending = PENDING.lock().unwrap_or_else(|error| error.into_inner());
    if EPOCH.load(Ordering::Acquire) == epoch {
        pending.epoch = epoch;
        pending.terms = terms.to_vec();
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
        let spawned = thread::Builder::new()
            .name("correction-watch".into())
            .spawn(move || {
                if let Some(field) = WatchedField::focused(pid) {
                    follow(self.0, &field, &inserted, save);
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start watching for corrections");
        }
    }
}

/// The reads a watch takes of its field.
trait Field {
    fn text(&self) -> Option<String>;
    fn caret(&self) -> Option<usize>;
    fn has_focus(&self) -> bool;
    fn in_front(&self) -> bool;
}

impl Field for WatchedField {
    fn text(&self) -> Option<String> {
        WatchedField::text(self)
    }

    fn caret(&self) -> Option<usize> {
        WatchedField::caret(self)
    }

    fn has_focus(&self) -> bool {
        WatchedField::has_focus(self)
    }

    fn in_front(&self) -> bool {
        WatchedField::in_front(self)
    }
}

fn follow(epoch: u64, field: &impl Field, inserted: &str, save: impl FnOnce(&[String])) {
    let current =
        || EPOCH.load(Ordering::Acquire) == epoch && crate::labs::learn_from_corrections();
    let started = Instant::now();
    let (anchor, mut last) = loop {
        if !current() || started.elapsed() > ARRIVAL {
            return;
        }
        if let Some(text) = field.text()
            && let Some(anchor) = anchor(&text, inserted, field.caret())
        {
            break (anchor, text);
        }
        thread::sleep(POLL);
    };
    let mut outcome = Outcome::Unchanged;
    while started.elapsed() < LONGEST_WATCH && current() && field.in_front() && field.has_focus() {
        thread::sleep(POLL);
        let Some(text) = field.text().filter(|text| !text.trim().is_empty()) else {
            break;
        };
        // Read before checking: a read that started after a newer paste, a
        // Return, or a dictation's start could already hold what they did.
        if !current() {
            break;
        }
        if text != last {
            outcome = taught(&anchor, inserted, &text);
            let terms = match &outcome {
                Outcome::Learned(terms) => terms.as_slice(),
                _ => &[],
            };
            publish(epoch, terms);
            last = text;
        }
    }
    publish(epoch, &[]);
    if !crate::labs::learn_from_corrections() {
        return;
    }
    match outcome {
        Outcome::Learned(terms) if !terms.is_empty() => save(&terms),
        outcome => tracing::debug!(
            outcome = outcome_name(&outcome),
            "correction watch learned nothing"
        ),
    }
}

/// What `field` teaches about the dictation `anchor` found in it.
fn taught(anchor: &Anchor, inserted: &str, field: &str) -> Outcome {
    match edited_region(anchor, field) {
        Some(edited) => learn(inserted, edited, anchor.open(), &vocabulary::entries()),
        None => Outcome::Outside,
    }
}

/// Adds `terms` to the vocabulary as saved right now, as `hex vocabulary
/// add` would, then has the app reload its settings. Existing entries are
/// never touched.
fn save(terms: &[String]) {
    let mut added = 0;
    let saved = AppSettings::edit(|settings| {
        added = add_learned(&mut settings.vocabulary, terms);
        added > 0
    });
    match saved {
        Ok(Some(_)) => {
            if let Some(runtime) = crate::macos_rune::runtime() {
                let _ = runtime.send(crate::macos_rune::Request::ReloadSettings);
            }
            tracing::info!(added, "learned vocabulary from a correction");
        }
        Ok(None) => tracing::debug!("learned vocabulary was already saved"),
        Err(error) => tracing::warn!(%error, "could not save learned vocabulary"),
    }
}

fn outcome_name(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Unchanged => "unchanged",
        Outcome::Outside => "outside",
        Outcome::TooLong => "too-long",
        Outcome::Rewrite => "rewrite",
        Outcome::Learned(_) => "nothing-new",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_settings::LabsSettings;
    use std::cell::{Cell, RefCell};

    /// The watch state and the lab switch are process-wide.
    static SERIAL: Mutex<()> = Mutex::new(());

    const INSERTED: &str = "send the deck to ronak today";
    const FIELD: &str = "Hi team, send the deck to ronak today. Thanks";
    const CORRECTED: &str = "Hi team, send the deck to Raunak today. Thanks";
    /// A read whose surrounding text changed: learning from it teaches
    /// nothing.
    const ELSEWHERE: &str = "Sent";

    /// A field whose reads come from `read`, called with each read's number
    /// from 1; the app stays in front until `front` turns off.
    struct Scripted<F> {
        read: RefCell<F>,
        count: Cell<usize>,
        front: Cell<bool>,
    }

    impl<F: FnMut(usize, &Cell<bool>) -> Option<String>> Field for Scripted<F> {
        fn text(&self) -> Option<String> {
            self.count.set(self.count.get() + 1);
            (self.read.borrow_mut())(self.count.get(), &self.front)
        }

        fn caret(&self) -> Option<usize> {
            None
        }

        fn has_focus(&self) -> bool {
            true
        }

        fn in_front(&self) -> bool {
            self.front.get()
        }
    }

    fn lab(on: bool) {
        crate::labs::apply(&LabsSettings {
            learn_from_corrections: on,
            ..LabsSettings::default()
        });
    }

    /// What a watch of [`INSERTED`] saves, its field read by `read`.
    fn saved(read: impl FnMut(usize, &Cell<bool>) -> Option<String>) -> Option<Vec<String>> {
        let field = Scripted {
            read: RefCell::new(read),
            count: Cell::new(0),
            front: Cell::new(true),
        };
        let mut saved = None;
        follow(begin().0, &field, INSERTED, |terms| {
            saved = Some(terms.to_vec());
        });
        saved
    }

    #[test]
    fn a_correction_is_saved_once_the_field_goes_away() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        lab(true);
        let saved = saved(|read, _| match read {
            1 => Some(FIELD.into()),
            2 => Some(CORRECTED.into()),
            _ => None,
        });
        assert_eq!(saved, Some(vec!["Raunak".into()]));
    }

    #[test]
    fn switching_apps_ends_the_watch_with_the_correction() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        lab(true);
        let saved = saved(|read, front| match read {
            1 => Some(FIELD.into()),
            2 => {
                front.set(false);
                Some(CORRECTED.into())
            }
            3 => Some(ELSEWHERE.into()),
            _ => None,
        });
        assert_eq!(saved, Some(vec!["Raunak".into()]));
    }

    #[test]
    fn turning_the_lab_off_mid_watch_saves_nothing() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        lab(true);
        let saved = saved(|read, _| match read {
            1 => Some(FIELD.into()),
            2 => Some(CORRECTED.into()),
            3 => {
                lab(false);
                Some(CORRECTED.into())
            }
            4 => panic!("the field is read after the lab turned off"),
            _ => None,
        });
        lab(false);
        assert_eq!(saved, None);
    }

    #[test]
    fn a_return_hex_presses_ends_the_watch() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        lab(true);
        let saved = saved(|read, _| match read {
            1 => Some(FIELD.into()),
            2 => Some(CORRECTED.into()),
            3 => {
                note_return();
                Some(ELSEWHERE.into())
            }
            4 => panic!("the field is read after Return"),
            _ => None,
        });
        assert_eq!(saved, Some(vec!["Raunak".into()]));
    }

    #[test]
    fn the_next_dictation_hears_a_correction_before_it_is_saved() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        lab(true);
        vocabulary::apply(&[], vocabulary::VocabularyBoost::Normal);
        let mut heard = Vec::new();
        let saved = saved(|read, _| match read {
            1 => Some(FIELD.into()),
            2 => Some(CORRECTED.into()),
            3 => {
                dictation_started();
                heard = vocabulary::terms();
                Some(ELSEWHERE.into())
            }
            _ => None,
        });
        vocabulary::apply(&[], vocabulary::VocabularyBoost::Normal);
        assert_eq!(heard, ["Raunak"], "in memory as the dictation starts");
        assert_eq!(saved, Some(vec!["Raunak".into()]));
        dictation_started();
        assert!(
            vocabulary::terms().is_empty(),
            "a finished watch hands nothing on"
        );
    }
}
