//! The live text plate (Labs): one click-through line
//! beside the recording HUD with what the dictation has said so far, on
//! every OS. It shows while a recording has text and hides when the
//! recording ends ([`crate::dictation::live_text`]); `rune_hud` says where the HUD is,
//! so the plate sits just past it, on the HUD's screen. Labs › Failure
//! notices reuses the same plate for one line saying why a dictation failed,
//! for a few seconds or until the next capture, and Labs › Learn from
//! corrections for one line naming what it added to the vocabulary, with an
//! Undo: the one part of the plate that takes a click. It never takes focus.
//!
//! The overlay is prewarmed: macOS keeps it drawn while hidden and refreshes
//! it before a reveal, so it never reappears with the last dictation's
//! words for a frame.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rune::{
    Anim, Cx, El, OverlayHandle, OverlayInput, OverlayOptions, OverlayPlacement, OverlaySpec,
    ScreenEdge, row, text,
};
use rune_kit::{Button, ButtonVariant, ControlSize};

use super::controls::Paint;
use crate::desktop::i18n::{tr, tr_fill};
use crate::dictation::corrections::Learned;
use crate::transcription::engine::Partial;

/// The overlay's size: room for about a dozen words on one line.
pub(crate) const SIZE: (f32, f32) = (560.0, 44.0);
/// The plate's height inside the overlay.
const PLATE_H: f32 = 32.0;
/// The overlay's room around the plate, across its height.
const PAD: f32 = (SIZE.1 - PLATE_H) / 2.0;
/// How long a notice stays beside the HUD.
const NOTICE_VISIBLE: Duration = Duration::from_secs(6);
/// Only a learned notice's Undo takes a click, and only macOS shows one.
/// Elsewhere the plate passes every click through, which keeps Rune's
/// XWayland fallback for it on GNOME.
const INPUT: OverlayInput = if cfg!(target_os = "macos") {
    OverlayInput::Regions
} else {
    OverlayInput::None
};

/// A one-line notice the plate shows for [`NOTICE_VISIBLE`].
#[derive(Clone)]
pub(crate) enum Notice {
    /// Labs › Failure notices: why the last dictation failed, shown instead
    /// of the live words.
    Failure(String),
    /// Labs › Learn from corrections: what a correction added, and what its
    /// Undo runs. Live words take the plate back from it.
    Learned(Learned, fn(&Learned)),
}

impl Notice {
    fn is_failure(&self) -> bool {
        matches!(self, Self::Failure(_))
    }
}

#[derive(Default)]
pub(crate) struct Plate {
    stable: String,
    tentative: String,
    notice: Option<Notice>,
    /// Whether live words are being published (a recording is showing text).
    live: bool,
    /// Whether the overlay is on screen. Every change happens in an overlay
    /// update, in queue order, so a queued hide cannot race a newer notice.
    shown: bool,
}

impl Plate {
    fn set_words(&mut self, stable: String, tentative: String) {
        self.stable = stable;
        self.tentative = tentative;
        self.live = true;
    }

    /// The recording stopped publishing words. Its last words stay drawn
    /// while the plate fades.
    fn words_ended(&mut self) {
        self.live = false;
    }

    fn show_notice(&mut self, notice: Notice) {
        self.notice = Some(notice);
    }

    fn take_notice(&mut self) -> bool {
        self.notice.take().is_some()
    }

    fn mark_shown(&mut self, shown: bool) {
        self.shown = shown;
    }

    /// Nothing live or noticed remains to justify the plate.
    fn should_hide(&self) -> bool {
        !self.live && self.notice.is_none()
    }
}

/// A notice and its deadline, kept apart from the drawn plate so a timer
/// thread can dismiss it without the live text's state.
#[derive(Default)]
struct NoticeSlot {
    message: Option<Notice>,
    expires_at: Option<Instant>,
}

impl NoticeSlot {
    /// Replaces any notice with `message` until [`NOTICE_VISIBLE`] from
    /// `now`; returns the new deadline.
    fn show(&mut self, message: Notice, now: Instant) -> Instant {
        self.message = Some(message);
        let deadline = now + NOTICE_VISIBLE;
        self.expires_at = Some(deadline);
        deadline
    }

    /// Dismisses the notice at once if `which` it is (a new capture
    /// dismisses any); whether one was showing.
    fn dismiss(&mut self, which: fn(&Notice) -> bool) -> bool {
        if !self.message.as_ref().is_some_and(which) {
            return false;
        }
        self.expires_at = None;
        self.message.take().is_some()
    }

    /// Dismisses a notice whose own deadline is due; whether it was showing.
    fn expire(&mut self, deadline: Instant, now: Instant) -> bool {
        if self.expires_at != Some(deadline) || now < deadline {
            return false;
        }
        self.expires_at = None;
        self.message.take().is_some()
    }
}

/// Where the plate starts, just past the HUD.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Spot {
    /// This far from the edge that the HUD is on, on the HUD's display
    /// (`rune_hud::display`).
    Edge(ScreenEdge, f32),
    /// At `y` in Rune's global space (points from the primary display's
    /// top-left, y down), centred on `x`; the plate hangs down from `y`
    /// when `below`, else rises from it.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Global { x: f32, y: f32, below: bool },
}

impl Spot {
    /// The overlay's placement for the plate to start at this spot.
    fn placement(self) -> OverlayPlacement {
        match self {
            Self::Edge(edge, margin) => OverlayPlacement::Edge {
                edge,
                margin: margin - PAD,
                align: rune::Align::Center,
                inset: 0.0,
            },
            Self::Global { x, y, below } => OverlayPlacement::Rect {
                x: x - SIZE.0 / 2.0,
                y: if below { y - PAD } else { y - SIZE.1 + PAD },
                width: SIZE.0,
                height: SIZE.1,
            },
        }
    }

    /// Places `overlay` for the plate to start at this spot.
    fn apply(self, overlay: &OverlayHandle<Plate>) {
        // Off macOS the HUD is placed with `place`, which picks the display
        // again and measures from the same edge of its work area (above the
        // Windows taskbar), so the plate is placed the same way and never
        // lands under the HUD (#797).
        #[cfg(not(target_os = "macos"))]
        if let Self::Edge(edge, margin) = self {
            overlay.place(edge, margin - PAD);
            return;
        }
        overlay.set_placement(Some(self.placement()));
    }
}

/// The plate's overlay, once installed.
static LIVE: std::sync::OnceLock<rune::OverlayHandle<Plate>> = std::sync::OnceLock::new();

/// Whether a non-blank partial has been queued since the last words-ended
/// update. It lets that update be skipped while nothing was ever shown,
/// without deciding visibility outside the overlay's queue.
static WORDS: AtomicBool = AtomicBool::new(false);

/// Where the plate goes, kept for a notice that appears after
/// installation.
static PLACE: OnceLock<Place> = OnceLock::new();

/// The current notice, which a timer thread dismisses.
static NOTICE: Mutex<NoticeSlot> = Mutex::new(NoticeSlot {
    message: None,
    expires_at: None,
});

/// The timer thread's queue: each newer notice's deadline supersedes the
/// one before it.
static TIMER: OnceLock<Sender<Instant>> = OnceLock::new();

fn notice() -> std::sync::MutexGuard<'static, NoticeSlot> {
    NOTICE.lock().unwrap_or_else(|error| error.into_inner())
}

/// Moves a shown plate to the display under the pointer when it changed
/// (`rune_hud`'s follow, #807).
#[cfg(not(target_os = "macos"))]
pub(crate) fn refollow_pointer() {
    if let Some(overlay) = LIVE.get() {
        overlay.refollow_pointer();
    }
}

/// Where the plate goes. Called on the app's thread each time the plate
/// appears, so it may read the screen.
pub(crate) type Place = fn() -> Spot;

/// Adds the plate to the app. It shows while a dictation publishes its text
/// (Labs › Live text) or while a failure notice explains a failed dictation
/// (Labs › Failure notices), applied at once. On macOS it's always prewarmed,
/// since either lab may be turned on after launch and an unprewarmed plate
/// reappears for a frame with the last dictation's words (#1075); elsewhere
/// when either is on at launch.
pub(crate) fn install<S: 'static>(app: &mut rune::App<S>, place: Place) {
    let spec = OverlaySpec {
        name: "Hex live text",
        size: SIZE,
        // Placed for real each time it appears.
        edge: ScreenEdge::Top,
        margin: 0.0,
        align: rune::Align::Center,
        inset: 0.0,
        interactive: false,
        view,
    }
    .options(OverlayOptions {
        input: INPUT,
        prewarm: cfg!(target_os = "macos")
            || crate::labs::live_text()
            || crate::labs::failure_notices(),
        #[cfg(not(target_os = "macos"))]
        display: crate::rune_hud::display(),
        ..Default::default()
    })
    // Hidden, it holds no GPU surface (nor, on Windows, a window), so it
    // never passes for a game to capture tools (#1308).
    .release_gpu_when_hidden();
    let overlay = app.overlay(spec, Plate::default());
    let _ = LIVE.set(overlay.clone());
    let _ = PLACE.set(place);
    crate::dictation::live_text::watch(move |partial| {
        let partial = partial.filter(|partial| !is_blank(partial));
        let Some(Partial { stable, tentative }) = partial.cloned() else {
            // Nothing was published since the last end and no notice is up:
            // this is a no-op, and skipping it keeps a dictation that never
            // showed text off the update queue.
            let words = WORDS.swap(false, Ordering::AcqRel);
            let notice_up = notice().message.is_some();
            if !words && !notice_up {
                return;
            }
            let handle = overlay.clone();
            overlay.update(move |plate, _| {
                plate.words_ended();
                if plate.should_hide() && plate.shown {
                    // Hidden from the same queue that shows it, so a recording
                    // that ends as its plate appears never leaves it showing.
                    // Its last words stay while it fades (macOS and Windows);
                    // the next appearance replaces them first.
                    handle.fade_out(FADE);
                    plate.mark_shown(false);
                }
            });
            return;
        };
        WORDS.store(true, Ordering::Release);
        let handle = overlay.clone();
        overlay.update(move |plate, tx| {
            plate.set_words(stable, tentative);
            tx.instant();
            if plate.notice.is_none() && !plate.shown {
                // Placed and shown after the new words, so it appears
                // where it belongs with them already drawn.
                place().apply(&handle);
                handle.show();
                plate.mark_shown(true);
            }
        });
    });
}

/// Shows `message` as a one-line notice beside the HUD for
/// [`NOTICE_VISIBLE`] (Labs › Failure notices), replacing any live words
/// until it leaves or a new capture starts.
pub(crate) fn show_failure(message: &str) {
    if !crate::labs::failure_notices() {
        return;
    }
    show(Notice::Failure(
        crate::desktop::activity::failure_explanation(message),
    ));
}

/// Says beside the HUD what a correction added to the vocabulary, with an
/// Undo that runs `undo` (Labs › Learn from corrections), until it leaves,
/// a new capture starts, or Undo is pressed.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn show_learned(learned: Learned, undo: fn(&Learned)) {
    if !crate::labs::learn_from_corrections() || learned.is_empty() {
        return;
    }
    show(Notice::Learned(learned, undo));
}

fn show(message: Notice) {
    let (Some(overlay), Some(place)) = (LIVE.get(), PLACE.get().copied()) else {
        return;
    };
    let deadline = notice().show(message.clone(), Instant::now());
    schedule(deadline);
    let handle = overlay.clone();
    overlay.update(move |plate, tx| {
        plate.show_notice(message);
        tx.instant();
        if !plate.shown {
            place().apply(&handle);
            handle.show();
            plate.mark_shown(true);
        }
    });
}

/// Hides a notice at once when a new capture starts; live words already on
/// the plate stay.
pub(crate) fn capture_started() {
    dismiss_notice(|_| true);
}

/// Hides a failure notice when Labs › Failure notices is turned off.
pub(crate) fn dismiss_failure() {
    dismiss_notice(Notice::is_failure);
}

/// Hides a learned notice when Labs › Learn from corrections is turned off,
/// from any settings surface.
pub(crate) fn dismiss_learned() {
    dismiss_notice(|notice| !notice.is_failure());
}

fn dismiss_notice(which: fn(&Notice) -> bool) {
    let cleared = { notice().dismiss(which) };
    if !cleared {
        return;
    }
    let Some(overlay) = LIVE.get() else {
        return;
    };
    let handle = overlay.clone();
    overlay.update(move |plate, tx| {
        // A notice that arrived after this dismissal stays.
        if notice().message.is_some() {
            return;
        }
        if plate.take_notice() {
            tx.instant();
        }
        if plate.should_hide() && plate.shown {
            handle.fade_out(FADE);
            plate.mark_shown(false);
        }
    });
}

/// Queues `deadline` to the timer thread, starting it on the first notice so
/// a Hex with the lab off runs no notice thread.
fn schedule(deadline: Instant) {
    let sender = TIMER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("hex-failure-notice".into())
            .spawn(move || run_timer(receiver));
        sender
    });
    let _ = sender.send(deadline);
}

fn run_timer(receiver: Receiver<Instant>) {
    while let Ok(mut deadline) = receiver.recv() {
        loop {
            let now = Instant::now();
            if now >= deadline {
                hide_expired(deadline);
                break;
            }
            match receiver.recv_timeout(deadline - now) {
                Ok(newer) => deadline = newer,
                Err(RecvTimeoutError::Timeout) => {
                    hide_expired(deadline);
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

fn hide_expired(deadline: Instant) {
    let expired = { notice().expire(deadline, Instant::now()) };
    if !expired {
        return;
    }
    let Some(overlay) = LIVE.get() else {
        return;
    };
    let handle = overlay.clone();
    overlay.update(move |plate, tx| {
        // A notice that arrived after this deadline stays.
        if notice().message.is_some() {
            return;
        }
        if plate.take_notice() {
            tx.instant();
        }
        if plate.should_hide() && plate.shown {
            handle.fade_out(FADE);
            plate.mark_shown(false);
        }
    });
}

/// How long the plate fades out once a dictation stops publishing.
const FADE: Duration = Duration::from_millis(150);

/// The model's first update often has no words yet.
fn is_blank(partial: &Partial) -> bool {
    partial.stable.trim().is_empty() && partial.tentative.trim().is_empty()
}

/// About one space of the plate's font, between right-to-left stable and
/// tentative words.
const RTL_SPACE: f32 = 4.0;

/// Whether the line's first strong letter is right-to-left (Hebrew, Arabic,
/// Persian, Syriac, Thaana, N'Ko, and their presentation forms).
fn is_right_to_left(stable: &str, tentative: &str) -> bool {
    stable
        .chars()
        .chain(tentative.chars())
        .find(|c| c.is_alphabetic() || is_rtl_letter(*c))
        .is_some_and(is_rtl_letter)
}

fn is_rtl_letter(c: char) -> bool {
    matches!(
        c,
        '\u{0590}'..='\u{08FF}' | '\u{FB1D}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFF}'
    )
}

fn plate(p: &Paint, line: El) -> El {
    let c = &p.k.c;
    row()
        .key("plate")
        .h(PLATE_H)
        .max_w(SIZE.0 - 8.0)
        .px(14.0)
        .center_y()
        .radius(PLATE_H / 2.0)
        .fill(c.bg_base)
        .border(p.hair, c.border_base)
        .child(line)
}

/// The Undo's room from the plate's end: its own padding keeps its label on
/// the plate's text inset.
const PLATE_END: f32 = 4.0;
/// Between the learned line's words and its Undo.
const UNDO_GAP: f32 = 4.0;

/// "Added Raunak to Vocabulary", or "Added 3 terms to Vocabulary", then
/// Undo. A long term gives way with "…" so the line stays one line and
/// keeps its ends.
fn learned_line(p: &Paint, learned: &Learned) -> El {
    let words = |key: &str, words: &str| {
        text(words.to_owned())
            .key(key)
            .font(p.k.ty.ui)
            .color(p.text())
    };
    let mut terms = learned.terms();
    let message = match (terms.len(), terms.next()) {
        (1, Some(term)) => {
            let template = tr("Added {} to Vocabulary");
            let (before, after) = template.split_once("{}").unwrap_or((template, ""));
            row()
                .key("learned-message")
                .min_w(0.0)
                .shrink()
                .child(words("learned-before", before))
                .child(words("learned-term", term).truncate().min_w(0.0).shrink())
                .child(words("learned-after", after))
        }
        (count, _) => row().key("learned-message").child(words(
            "learned-count",
            &tr_fill("Added {} terms to Vocabulary", &count.to_string()),
        )),
    };
    let undo = Button::new(tr("Undo"))
        .variant(ButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .on_press(undo_pressed)
        .build(p.k)
        .key("undo")
        .hit();
    row()
        .key("learned")
        .min_w(0.0)
        .shrink()
        .center_y()
        .gap(UNDO_GAP)
        .child(message)
        .child(undo)
}

/// Runs the learned notice's Undo once, and the notice leaves. Nothing runs
/// once the notice was dismissed and its leaving is still queued: the lab
/// turned off, or the vocabulary changed since.
fn undo_pressed(plate: &mut Plate) {
    let Some(Notice::Learned(learned, undo)) = plate.notice.take() else {
        return;
    };
    let still_showing = notice().dismiss(|notice| !notice.is_failure());
    if still_showing && crate::labs::learn_from_corrections() {
        undo(&learned);
    }
    if plate.should_hide() && plate.shown {
        if let Some(overlay) = LIVE.get() {
            overlay.fade_out(FADE);
        }
        plate.mark_shown(false);
    }
}

fn view(plate_state: &Plate, cx: &mut Cx<Plate>) -> El {
    let p = Paint::new(cx.system_dark(), cx.env().scale);
    let root = row()
        .key("live-text")
        .w_full()
        .h_full()
        .center()
        .px(4.0)
        .hover_motion_within(Anim::INSTANT, Anim::INSTANT);
    match &plate_state.notice {
        Some(Notice::Failure(failure)) => {
            let line = text(failure.clone())
                .key("failure")
                .truncate()
                .font(p.k.ty.ui)
                .color(p.danger());
            return root.child(plate(&p, line));
        }
        Some(Notice::Learned(learned, _)) if !plate_state.live => {
            return root.child(plate(&p, learned_line(&p, learned)).pr(PLATE_END));
        }
        _ => {}
    }
    if plate_state.stable.trim().is_empty() && plate_state.tentative.trim().is_empty() {
        return root;
    }
    // The newest words stay in view: the start gives way with "…".
    let stable = text(plate_state.stable.trim().to_owned())
        .key("stable")
        .truncate_head()
        .font(p.k.ty.ui)
        .color(p.text());
    let rtl = is_right_to_left(&plate_state.stable, &plate_state.tentative);
    let tentative = (!plate_state.tentative.trim().is_empty()).then(|| {
        let words = text(if rtl {
            // A leading space would sit on the right-to-left line's far
            // (right) side; the gap goes between the words instead.
            plate_state.tentative.trim().to_owned()
        } else {
            plate_state.tentative.clone()
        })
        .key("tentative")
        .truncate()
        .font(p.k.ty.ui)
        .color(p.faint());
        let spaced = rtl
            && !plate_state.stable.trim().is_empty()
            && plate_state.tentative.starts_with(char::is_whitespace);
        if spaced { words.mr(RTL_SPACE) } else { words }
    });
    let line = row().key("line").min_w(0.0).shrink();
    // Right-to-left, the newer (tentative) words read after the stable ones,
    // so they sit to their left.
    let line = if rtl {
        line.children(tentative).child(stable)
    } else {
        line.child(stable).children(tentative)
    };
    root.child(plate(&p, line))
}

#[cfg(test)]
mod tests {
    use rune::Runtime;
    use rune::text::TextSystem;

    use super::*;

    const SHORT: &str = "So the plan for today";
    const LONG: &str = "So the plan for today is to finish the release notes, then check the \
        streaming numbers on the M4, and after lunch we can review the onboarding flow together";
    const HEBREW: &str = "אז התוכנית להיום";

    /// A term as long as the vocabulary takes.
    const LONGEST_TERM: &str = "Raunak Chowdhuri-Featherstonehaugh of the Platform Infrastructur";

    fn failure(message: &str) -> Notice {
        Notice::Failure(message.into())
    }

    /// The failure `slot` shows, if any.
    fn said(slot: &NoticeSlot) -> Option<&str> {
        match &slot.message {
            Some(Notice::Failure(message)) => Some(message),
            _ => None,
        }
    }

    fn learned(terms: &[&str]) -> Notice {
        let terms: Vec<String> = terms.iter().map(|term| term.to_string()).collect();
        Notice::Learned(
            crate::dictation::corrections::add_learned(&mut Vec::new(), &terms),
            |_| {},
        )
    }

    #[test]
    fn a_failure_notice_lasts_six_seconds() {
        let start = Instant::now();
        let mut notice = NoticeSlot::default();
        let deadline = notice.show(
            failure("No microphone is available. Connect a microphone."),
            start,
        );
        assert_eq!(
            said(&notice),
            Some("No microphone is available. Connect a microphone.")
        );
        assert!(!notice.expire(deadline, start + NOTICE_VISIBLE - Duration::from_millis(1)));
        assert!(notice.expire(deadline, start + NOTICE_VISIBLE));
        assert!(notice.message.is_none());
    }

    #[test]
    fn a_new_capture_dismisses_a_failure_notice_at_once() {
        let start = Instant::now();
        let mut notice = NoticeSlot::default();
        let deadline = notice.show(failure("Could not open microphone"), start);
        assert!(notice.dismiss(|_| true));
        assert!(notice.message.is_none());
        assert!(!notice.dismiss(|_| true));
        assert!(
            !notice.expire(deadline, start + NOTICE_VISIBLE),
            "the old deadline cannot dismiss a later notice"
        );
    }

    #[test]
    fn only_a_notices_own_deadline_dismisses_it() {
        let start = Instant::now();
        let mut notice = NoticeSlot::default();
        let older = notice.show(failure("first"), start);
        let newer = notice.show(failure("second"), start + Duration::from_secs(1));
        assert!(!notice.expire(older, start + NOTICE_VISIBLE));
        assert_eq!(said(&notice), Some("second"));
        assert!(notice.expire(newer, start + Duration::from_secs(1) + NOTICE_VISIBLE));
        assert!(notice.message.is_none());
    }

    fn runtime(stable: &str, tentative: &str, dark: bool) -> Runtime<Plate> {
        runtime_plate(
            Plate {
                stable: stable.into(),
                tentative: tentative.into(),
                ..Plate::default()
            },
            dark,
        )
    }

    fn runtime_plate(plate: Plate, dark: bool) -> Runtime<Plate> {
        let mut rt = crate::rune_ui::offscreen::runtime(
            plate,
            view,
            SIZE,
            rune::Env {
                system_dark: dark,
                scale: 2.0,
            },
            TextSystem::new(rune_theme_opencode::FONTS, rune_theme_opencode::FAMILIES),
        );
        rt.settle_all();
        rt.frame(0.0);
        rt
    }

    fn rect(rt: &Runtime<Plate>, key: &str) -> Option<[f32; 4]> {
        rt.elements()
            .into_iter()
            .find(|e| !e.exiting && e.key.as_deref() == Some(key))
            .map(|e| e.rect)
    }

    #[test]
    fn a_short_line_hugs_its_words_in_the_middle() {
        let rt = runtime(SHORT, "", false);
        let [x, y, w, h] = rect(&rt, "plate").expect("the plate draws");
        assert!(w < SIZE.0 / 2.0, "the plate hugs a short line: {w}");
        assert!((x + w / 2.0 - SIZE.0 / 2.0).abs() < 1.0, "centred: {x} {w}");
        assert!((y + h / 2.0 - SIZE.1 / 2.0).abs() < 1.0, "centred: {y} {h}");
        assert!(rect(&rt, "tentative").is_none());
    }

    #[test]
    fn a_long_line_stays_inside_the_overlay_on_one_line() {
        for tentative in ["", " together with the team"] {
            let rt = runtime(LONG, tentative, true);
            let [x, _, w, h] = rect(&rt, "plate").unwrap();
            assert!(x >= 0.0 && x + w <= SIZE.0, "inside: {x} {w}");
            assert_eq!(h, PLATE_H);
            let [line_x, _, line_w, _] = rect(&rt, "line").unwrap();
            assert!(
                line_x + line_w <= x + w,
                "the words fit the plate: {line_x} {line_w}"
            );
            let [_, _, _, line] = rect(&rt, "stable").unwrap();
            assert!(line < PLATE_H, "one line: {line}");
            assert_eq!(rect(&rt, "tentative").is_some(), !tentative.is_empty());
        }
    }

    #[test]
    fn tentative_words_follow_the_stable_ones_in_reading_order() {
        let rt = runtime(SHORT, " is to finish", false);
        let [sx, _, sw, _] = rect(&rt, "stable").unwrap();
        let [tx, _, _, _] = rect(&rt, "tentative").unwrap();
        assert!(tx >= sx + sw - 0.5, "left to right: {sx} {sw} {tx}");

        let rt = runtime(HEBREW, " והיום", false);
        let [sx, _, _, _] = rect(&rt, "stable").unwrap();
        let [tx, _, tw, _] = rect(&rt, "tentative").unwrap();
        assert!(tx + tw < sx, "right to left, after a space: {tx} {tw} {sx}");

        assert!(is_right_to_left(HEBREW, ""));
        assert!(is_right_to_left("  ", " مرحبا"));
        assert!(!is_right_to_left(SHORT, " שלום"));
        assert!(!is_right_to_left("", ""));
    }

    #[test]
    fn a_spot_places_the_plate_just_past_the_hud() {
        assert_eq!(
            Spot::Edge(ScreenEdge::Bottom, 50.0).placement(),
            OverlayPlacement::Edge {
                edge: ScreenEdge::Bottom,
                margin: 50.0 - PAD,
                align: rune::Align::Center,
                inset: 0.0,
            }
        );
        let rect = |below| match (Spot::Global {
            x: 700.0,
            y: 100.0,
            below,
        })
        .placement()
        {
            OverlayPlacement::Rect { x, y, width, .. } => (x, y, width),
            other => panic!("{other:?}"),
        };
        // Centred on the HUD; the plate's top edge at the spot when hanging
        // below it, its bottom edge at the spot when rising above it.
        assert_eq!(rect(true), (700.0 - SIZE.0 / 2.0, 100.0 - PAD, SIZE.0));
        assert_eq!(
            rect(false),
            (700.0 - SIZE.0 / 2.0, 100.0 - PAD - PLATE_H, SIZE.0)
        );
    }

    #[test]
    fn a_failure_arriving_after_a_queued_words_end_stays_visible() {
        let mut plate = Plate::default();
        plate.set_words("old words".into(), String::new());
        plate.mark_shown(true);

        // A queued words-ended update runs first and would hide an empty
        // plate, but the failure queued behind it brings the plate back.
        plate.words_ended();
        assert!(plate.should_hide());
        plate.show_notice(failure("No microphone is available."));
        assert!(!plate.should_hide());
        assert_eq!(plate.stable, "old words", "the old words stay for the fade");

        // Once the notice is up, a later words end cannot hide it.
        plate.words_ended();
        assert!(!plate.should_hide());
        assert!(plate.take_notice());
        assert!(plate.should_hide());
        plate.mark_shown(false);
    }

    #[test]
    fn a_failure_notice_takes_the_plate_from_the_live_words() {
        let rt = runtime_plate(
            Plate {
                stable: SHORT.into(),
                tentative: " and then".into(),
                notice: Some(failure("No microphone is available. Connect a microphone.")),
                ..Plate::default()
            },
            false,
        );
        assert!(rect(&rt, "plate").is_some());
        assert!(rect(&rt, "failure").is_some());
        assert!(rect(&rt, "stable").is_none());
        assert!(rect(&rt, "tentative").is_none());
    }

    #[test]
    fn a_learned_notice_names_one_term_or_counts_several_on_one_line() {
        for (terms, message) in [
            (&["Raunak"][..], "Added Raunak to Vocabulary"),
            (
                &["Raunak", "OpenCode", "Zed"][..],
                "Added 3 terms to Vocabulary",
            ),
        ] {
            let rt = runtime_plate(
                Plate {
                    notice: Some(learned(terms)),
                    ..Plate::default()
                },
                true,
            );
            let shown: String = rt
                .elements()
                .into_iter()
                .filter(|e| {
                    e.key
                        .as_deref()
                        .is_some_and(|key| key.starts_with("learned-") && key != "learned-message")
                })
                .filter_map(|e| e.text.clone())
                .collect();
            assert_eq!(shown, message);
            assert!(rect(&rt, "undo").is_some(), "with its Undo");
        }
        // The longest term gives way inside the plate rather than wrapping,
        // and "to Vocabulary" and Undo stay.
        let rt = runtime_plate(
            Plate {
                notice: Some(learned(&[LONGEST_TERM])),
                ..Plate::default()
            },
            false,
        );
        assert!(rect(&rt, "learned-after").is_some());
        let [x, _, w, h] = rect(&rt, "plate").unwrap();
        assert!(x >= 0.0 && x + w <= SIZE.0, "inside: {x} {w}");
        assert_eq!(h, PLATE_H);
        let [ux, _, uw, _] = rect(&rt, "undo").unwrap();
        assert!(ux + uw <= x + w, "Undo stays on the plate: {ux} {uw}");
        let [_, _, _, line] = rect(&rt, "learned-term").unwrap();
        assert!(line < PLATE_H, "one line: {line}");
    }

    #[test]
    fn live_words_take_the_plate_from_a_learned_notice() {
        let mut plate = Plate {
            notice: Some(learned(&["Raunak"])),
            ..Plate::default()
        };
        plate.set_words(SHORT.into(), String::new());
        let rt = runtime_plate(plate, false);
        assert!(rect(&rt, "stable").is_some());
        assert!(rect(&rt, "learned").is_none());
    }

    fn lab(on: bool) {
        crate::labs::apply(&crate::app_settings::LabsSettings {
            learn_from_corrections: on,
            ..Default::default()
        });
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn undo_runs_once_and_the_notice_leaves() {
        let _shared = crate::dictation::corrections::test_state();
        static UNDONE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        lab(true);
        let terms = ["Raunak".to_string()];
        let shown = Notice::Learned(
            crate::dictation::corrections::add_learned(&mut Vec::new(), &terms),
            |learned| {
                assert_eq!(learned.terms().collect::<Vec<_>>(), ["Raunak"]);
                UNDONE.fetch_add(1, Ordering::SeqCst);
            },
        );
        notice().show(shown.clone(), Instant::now());
        let mut plate = Plate {
            notice: Some(shown),
            ..Plate::default()
        };
        undo_pressed(&mut plate);
        undo_pressed(&mut plate);
        lab(false);
        assert_eq!(UNDONE.load(Ordering::SeqCst), 1);
        assert!(plate.notice.is_none());
        assert!(plate.should_hide());
    }

    #[test]
    fn undo_does_nothing_once_its_term_was_removed_and_added_again() {
        let _shared = crate::dictation::corrections::test_state();
        use crate::dictation::vocabulary::{self, VocabularyBoost, VocabularyEntry};
        let entry = |term: &str| VocabularyEntry {
            term: term.into(),
            sounds_like: Vec::new(),
        };
        lab(true);
        let mut list = vec![entry("OpenCode")];
        let shown = Notice::Learned(
            crate::dictation::corrections::add_learned(&mut list, &["Raunak".into()]),
            |_| panic!("Undo removed the user's own Raunak"),
        );
        vocabulary::apply(&list, VocabularyBoost::Normal);
        notice().show(shown.clone(), Instant::now());
        let mut plate = Plate {
            notice: Some(shown),
            ..Plate::default()
        };
        // In the Vocabulary pane, or `hex vocabulary remove` then `add`:
        // each save applies the list, and the last one reads as the first.
        vocabulary::apply(&[entry("OpenCode")], VocabularyBoost::Normal);
        vocabulary::apply(&list, VocabularyBoost::Normal);
        assert!(notice().message.is_none(), "the notice leaves");
        undo_pressed(&mut plate);
        vocabulary::apply(&[], VocabularyBoost::Normal);
        lab(false);
    }

    #[test]
    fn undo_does_nothing_once_its_lab_is_off() {
        let _shared = crate::dictation::corrections::test_state();
        lab(false);
        let mut plate = Plate {
            notice: Some(Notice::Learned(
                crate::dictation::corrections::add_learned(&mut Vec::new(), &["Raunak".into()]),
                |_| panic!("Undo ran with its lab off"),
            )),
            ..Plate::default()
        };
        undo_pressed(&mut plate);
        assert!(plate.notice.is_none(), "the notice still leaves");
    }

    #[test]
    fn turning_the_lab_off_anywhere_dismisses_a_learned_notice() {
        let _shared = crate::dictation::corrections::test_state();
        notice().show(learned(&["Raunak"]), Instant::now());
        // As `hex settings set labs.learn_from_corrections false` saves it.
        lab(false);
        assert!(notice().message.is_none());
    }

    #[test]
    fn off_macos_the_plate_passes_every_click_through() {
        assert_eq!(INPUT == OverlayInput::Regions, cfg!(target_os = "macos"));
    }

    #[test]
    fn no_words_draw_nothing() {
        let rt = runtime("  ", "", false);
        assert!(rect(&rt, "plate").is_none());
        let blank = |stable: &str, tentative: &str| {
            is_blank(&Partial {
                stable: stable.into(),
                tentative: tentative.into(),
            })
        };
        assert!(blank("", ""));
        assert!(blank("  ", " "));
        assert!(!blank("", " so"));
        assert!(!blank("so", ""));
    }

    /// Renders the plate, its live words and its learned notices, to
    /// `HEX_LIVE_TEXT_SNAPS/live-text-*.png` in both appearances, to look at.
    #[test]
    #[ignore = "set HEX_LIVE_TEXT_SNAPS to a folder; needs a GPU"]
    fn snapshots() {
        let folder = std::path::PathBuf::from(std::env::var("HEX_LIVE_TEXT_SNAPS").unwrap());
        let gpu = rune::render::Gpu::try_new(rune::render::Gpu::instance(), None).unwrap();
        fn words(stable: &str, tentative: &str) -> Plate {
            Plate {
                stable: stable.into(),
                tentative: tentative.into(),
                ..Plate::default()
            }
        }
        fn notice(notice: Notice) -> Plate {
            Plate {
                notice: Some(notice),
                ..Plate::default()
            }
        }
        type MakePlate = fn() -> Plate;
        let plates: [(&str, MakePlate); 7] = [
            ("short", || words(SHORT, "")),
            ("long", || words(LONG, "")),
            ("tentative", || words(SHORT, " is to finish")),
            ("rtl", || words(HEBREW, " והיום")),
            ("learned-one", || notice(learned(&["Raunak"]))),
            ("learned-several", || {
                notice(learned(&["Raunak", "OpenCode", "Zed"]))
            }),
            ("learned-long", || notice(learned(&[LONGEST_TERM]))),
        ];
        for (name, plate) in plates {
            for dark in [false, true] {
                let mut rt = runtime_plate(plate(), dark);
                let (w, h) = ((SIZE.0 * 2.0) as u32, (SIZE.1 * 2.0) as u32);
                let mut renderer = crate::rune_ui::offscreen::renderer(&gpu.device);
                let target = crate::rune_ui::offscreen::target(&gpu.device, w, h);
                renderer.upload_images(&gpu.device, &gpu.queue, &mut rt.images);
                renderer.prepare(
                    &gpu.device,
                    &gpu.queue,
                    &mut rt.text.atlas,
                    &rt.instances,
                    (w, h),
                );
                let mut encoder = gpu.device.create_command_encoder(&Default::default());
                // A desktop grey behind the clear overlay.
                let backdrop = if dark { 0.08 } else { 0.62 };
                renderer.draw(
                    &mut encoder,
                    &target.view,
                    [backdrop, backdrop, backdrop, 1.0],
                );
                target.copy(&mut encoder);
                gpu.queue.submit([encoder.finish()]);
                let theme = if dark { "dark" } else { "light" };
                let path = folder.join(format!("live-text-{name}-{theme}.png"));
                rune::render::write_png(&path, w, h, &target.read(&gpu.device)).unwrap();
            }
        }
    }
}
