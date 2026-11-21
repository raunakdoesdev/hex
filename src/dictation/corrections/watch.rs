//! Follows the field a dictation landed in until the user is done with it,
//! then saves what [`super::learn`] finds to the vocabulary.
//!
//! The watch ends at Return (typed, or pressed by YOLO mode), the app or the
//! field losing focus, a field that disappears or is emptied, a newer
//! insertion, the next capture becoming real, the lab turning off, or five
//! minutes. It learns from the last read of the field taken before any of
//! those, so a message sent with Return or a newer dictation pasted on top
//! never counts as the user's edit, and a lab turned off saves nothing.
//! Nothing here logs the text.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::{Anchor, Learned, Outcome, add_learned, anchor, edited_region, learn};
use crate::accessibility::WatchedField;
use crate::app_settings::AppSettings;
use crate::dictation::vocabulary::{self, VocabularyEntry};

const POLL: Duration = Duration::from_millis(100);
const LONGEST_WATCH: Duration = Duration::from_secs(5 * 60);
/// How long the pasted text has to show up in the field before the watch
/// gives up on it: a paste lands asynchronously, and some apps never expose
/// their text.
const ARRIVAL: Duration = Duration::from_secs(2);

/// Bumped by everything that ends a watch: an insertion starting, Return,
/// and a capture becoming real.
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

/// A dictation's shortcut went down, before its audio starts: the terms the
/// watch's last read already taught join the vocabulary in memory, so this
/// capture's stream hears them. The watch keeps going, since the press may
/// be a chord such as Option–Left on the way to the word being fixed; it
/// ends at [`capture_intended`]. Nothing here reads Accessibility or
/// settings.
pub(crate) fn dictation_pressed() {
    let terms = {
        let pending = PENDING.lock().unwrap_or_else(|error| error.into_inner());
        if pending.epoch == EPOCH.load(Ordering::Acquire) {
            pending.terms.clone()
        } else {
            Vec::new()
        }
    };
    if !terms.is_empty() && crate::labs::learn_from_corrections() {
        vocabulary::add_unsaved(&terms);
    }
}

/// The capture is real (a hold past its threshold, a control or API start,
/// or a released microphone opening): the watch ends and saves what its
/// last read taught on its own thread.
pub(crate) fn capture_intended() {
    let _pending = PENDING.lock().unwrap_or_else(|error| error.into_inner());
    end();
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
    // What was known before this watch: a chord press adds the watch's own
    // unsaved terms to the vocabulary in memory, and those still need saving.
    let known = vocabulary::entries();
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
    let watching = || current() && field.in_front() && field.has_focus();
    let mut outcome = Outcome::Unchanged;
    while started.elapsed() < LONGEST_WATCH {
        thread::sleep(POLL);
        let text = field.text();
        // A read counts only if the watch still stands once it returns:
        // whatever ended it (a newer paste, a Return, a capture, another
        // app in front) may have happened at any point before, the read
        // included, so its text could hold what they did.
        if !watching() {
            break;
        }
        let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
            break;
        };
        if text != last {
            outcome = taught(&anchor, inserted, &text, &known);
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

/// What `field` teaches about the dictation `anchor` found in it, beyond
/// the `known` vocabulary.
fn taught(anchor: &Anchor, inserted: &str, field: &str, known: &[VocabularyEntry]) -> Outcome {
    match edited_region(anchor, field) {
        Some(edited) => learn(inserted, edited, anchor.open(), known),
        None => Outcome::Outside,
    }
}

/// Adds `terms` to the vocabulary as saved right now, as `hex vocabulary
/// add` would, has the app reload its settings, and says what it added
/// beside the HUD with an Undo. Existing entries are never touched.
fn save(terms: &[String]) {
    let mut learned = Learned::default();
    let saved = AppSettings::edit(|settings| {
        learned = add_learned(&mut settings.vocabulary, terms);
        !learned.is_empty()
    });
    match saved {
        Ok(Some(_)) => {
            reload_settings();
            tracing::info!(
                added = learned.terms().len(),
                "learned vocabulary from a correction"
            );
            crate::rune_ui::live_text::show_learned(learned, undo);
        }
        Ok(None) => tracing::debug!("learned vocabulary was already saved"),
        Err(error) => tracing::warn!(%error, "could not save learned vocabulary"),
    }
}

/// The notice's Undo: takes `learned` back out of the vocabulary as saved
/// right now, under the same lock as its save, unless the vocabulary has
/// changed since.
fn undo(learned: &Learned) {
    let mut removed = 0;
    match AppSettings::edit(|settings| {
        removed = learned.undo(&mut settings.vocabulary);
        removed > 0
    }) {
        Ok(Some(_)) => {
            reload_settings();
            tracing::info!(removed, "undid learned vocabulary");
        }
        Ok(None) => tracing::debug!("learned vocabulary was already changed or removed"),
        Err(error) => tracing::warn!(%error, "could not undo learned vocabulary"),
    }
}

fn reload_settings() {
    if let Some(runtime) = crate::macos_rune::runtime() {
        let _ = runtime.send(crate::macos_rune::Request::ReloadSettings);
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
    /// from 1; the app stays in front until `front` turns off, or until the
    /// check that follows read `leave_after` has seen it in front.
    struct Scripted<F> {
        read: RefCell<F>,
        count: Cell<usize>,
        front: Cell<bool>,
        leave_after: Option<usize>,
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
            let front = self.front.get();
            if self.leave_after == Some(self.count.get()) {
                self.front.set(false);
            }
            front
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
        saved_leaving(None, read)
    }

    /// [`saved`], with the app leaving the front right after the check that
    /// follows read `leave_after`.
    fn saved_leaving(
        leave_after: Option<usize>,
        read: impl FnMut(usize, &Cell<bool>) -> Option<String>,
    ) -> Option<Vec<String>> {
        let field = Scripted {
            read: RefCell::new(read),
            count: Cell::new(0),
            front: Cell::new(true),
            leave_after,
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
        // The switch lands after the check that passed read 2 and before
        // read 3 is taken: the gap between one poll's check and the next.
        let saved = saved_leaving(Some(2), |read, front| match read {
            1 => Some(FIELD.into()),
            2 => Some(CORRECTED.into()),
            3 => {
                assert!(!front.get(), "read 3 is taken after the switch");
                Some(ELSEWHERE.into())
            }
            4 => panic!("the field is read after its app left the front"),
            _ => None,
        });
        assert_eq!(saved, Some(vec!["Raunak".into()]));
    }

    #[test]
    fn an_unchanged_dictation_saves_nothing() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        lab(true);
        let saved = saved(|read, _| (read < 4).then(|| FIELD.into()));
        assert_eq!(saved, None, "no save, so no notice");
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
                dictation_pressed();
                heard = vocabulary::terms();
                capture_intended();
                Some(ELSEWHERE.into())
            }
            4 => panic!("the field is read after the capture became real"),
            _ => None,
        });
        vocabulary::apply(&[], vocabulary::VocabularyBoost::Normal);
        assert_eq!(heard, ["Raunak"], "in memory as the shortcut goes down");
        assert_eq!(saved, Some(vec!["Raunak".into()]));
        dictation_pressed();
        assert!(
            vocabulary::terms().is_empty(),
            "a finished watch hands nothing on"
        );
    }

    #[test]
    fn a_press_that_never_becomes_a_capture_keeps_the_watch() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        lab(true);
        // Option–Left on the way to the misspelling: the press is a chord.
        let saved = saved(|read, _| match read {
            1 => Some(FIELD.into()),
            2 => {
                dictation_pressed();
                Some(FIELD.into())
            }
            3 => Some(CORRECTED.into()),
            _ => None,
        });
        assert_eq!(saved, Some(vec!["Raunak".into()]));
    }

    #[test]
    fn a_correction_heard_on_a_chord_is_still_saved_after_a_later_edit() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        lab(true);
        vocabulary::apply(&[], vocabulary::VocabularyBoost::Normal);
        // Option–Right past the fixed name, then a word typed at the end.
        let saved = saved(|read, _| match read {
            1 => Some(FIELD.into()),
            2 => Some(CORRECTED.into()),
            3 => {
                dictation_pressed();
                Some("Hi team, send the deck to Raunak today please. Thanks".into())
            }
            _ => None,
        });
        vocabulary::apply(&[], vocabulary::VocabularyBoost::Normal);
        assert_eq!(
            saved,
            Some(vec!["Raunak".into()]),
            "saved, so the notice shows"
        );
    }
}
