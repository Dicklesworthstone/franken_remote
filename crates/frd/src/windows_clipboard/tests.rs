use super::*;
use fr_core::clipboard::Endpoint;
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// The Win32 clipboard as the owner sees it: the number is checked once
/// open, a write is five changes (empty, set, three synthesized formats).
#[derive(Default)]
struct Board {
    sequence: u32,
    text: Option<String>,
    /// Native calls of any kind.
    calls: usize,
    closed: usize,
    /// Another program copies this between the owner's last look and the
    /// write's open.
    race: Option<&'static str>,
    /// Another program empties the clipboard after the write, before its
    /// number is read.
    stolen: bool,
    /// The clipboard's text is not valid UTF-16.
    invalid: bool,
}

#[derive(Clone, Default)]
struct Fake(Rc<RefCell<Board>>);

impl Fake {
    fn copy(&self, text: &str) {
        let mut board = self.0.borrow_mut();
        board.sequence += 4;
        board.text = Some(text.to_owned());
        board.invalid = false;
    }

    fn text(&self) -> Option<String> {
        self.0.borrow().text.clone()
    }

    fn calls(&self) -> usize {
        self.0.borrow().calls
    }
}

struct Owned(String);

impl AsRef<str> for Owned {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Native for Fake {
    type Text = Owned;

    fn sequence(&mut self) -> u32 {
        let mut board = self.0.borrow_mut();
        board.calls += 1;
        board.sequence
    }

    fn has_text(&mut self) -> bool {
        let mut board = self.0.borrow_mut();
        board.calls += 1;
        board.text.is_some()
    }

    fn read(
        &mut self,
        limit: usize,
        expected: u32,
        allowed: &dyn Fn() -> bool,
    ) -> Result<Owned, Failure> {
        let mut board = self.0.borrow_mut();
        board.calls += 1;
        if board.sequence != expected {
            return Err(Failure::Changed);
        }
        if !allowed() {
            return Err(Failure::Refused);
        }
        if board.invalid {
            return Err(Failure::Invalid);
        }
        let text = board.text.clone().ok_or(Failure::NoText)?;
        if text.len() > limit {
            return Err(Failure::Limit);
        }
        Ok(Owned(text))
    }

    fn write(
        &mut self,
        text: &str,
        expected: u32,
        allowed: &dyn Fn() -> bool,
    ) -> Result<u32, (Failure, bool)> {
        let race = self.0.borrow_mut().race.take();
        if let Some(race) = race {
            self.copy(race);
        }
        let mut board = self.0.borrow_mut();
        board.calls += 1;
        if board.sequence != expected {
            return Err((Failure::Changed, false));
        }
        if !allowed() {
            return Err((Failure::Refused, false));
        }
        board.sequence += 5;
        board.text = Some(text.to_owned());
        board.invalid = false;
        if board.stolen {
            board.sequence += 4;
            board.text = Some("theirs".to_owned());
            return Err((Failure::Changed, true));
        }
        Ok(board.sequence)
    }

    fn close(&mut self) {
        self.0.borrow_mut().closed += 1;
    }
}

fn stamp(sequence: u64) -> Stamp {
    Stamp {
        id: u128::from(sequence),
        source: Endpoint::Controller,
        sequence,
    }
}

const UNTIL: HostInstant = HostInstant::from_micros(100);

fn owner(
    fake: &Fake,
    gate: impl Fn(Option<HostInstant>) -> bool + Send + Sync + 'static,
) -> Clipboard<Fake> {
    Clipboard::open(fake.clone(), &ProtocolLimits::ABSOLUTE, gate).unwrap()
}

/// The next `changes` samples now instead of waiting out the 50 ms poll.
fn refresh(clipboard: &mut Clipboard<Fake>) -> NativeChanges {
    clipboard.last_poll = None;
    clipboard.changes().unwrap()
}

fn read(clipboard: &mut Clipboard<Fake>) -> Result<Text<Owned>, Error> {
    clipboard.begin_read()?;
    Ok(clipboard.poll_read()?.unwrap())
}

#[test]
fn nothing_native_runs_before_the_gate_opens_and_a_paused_view_defers() {
    let fake = Fake::default();
    let enabled = Arc::new(AtomicBool::new(false));
    let gate = enabled.clone();
    let mut clipboard = owner(&fake, move |_| gate.load(Ordering::Acquire));
    assert_eq!(clipboard.watch(), Err(Error::Access));
    assert_eq!(fake.calls(), 0);
    enabled.store(true, Ordering::Release);
    assert!(clipboard.watch().unwrap() > 0);
    assert!(fake.calls() > 0);
    enabled.store(false, Ordering::Release);
    let before = fake.calls();
    let paused = refresh(&mut clipboard);
    assert!(paused.latest.is_none());
    assert!(!paused.settled);
    assert_eq!(fake.calls(), before);
    enabled.store(true, Ordering::Release);
    assert!(refresh(&mut clipboard).settled);
}

#[test]
fn the_controllers_text_lands_whole_and_reads_back_with_its_stamp() {
    let fake = Fake::default();
    fake.copy("local");
    let mut clipboard = owner(&fake, |_| true);
    clipboard.watch().unwrap();
    for (sequence, text) in [(1, "h\u{e9}llo \u{1d11e}\n\u{3bb}"), (2, "")] {
        let original = stamp(sequence);
        let revision = clipboard.revision();
        clipboard
            .prepare_for_revision(text, original, revision)
            .unwrap();
        assert_eq!(
            clipboard.publish_until(text, original, UNTIL),
            Publication::SubmittedToOs
        );
        assert_eq!(fake.text().as_deref(), Some(text));
        let change = clipboard.changes().unwrap().latest.unwrap();
        assert!(change.has_selection);
        assert_eq!(change.origin, Some(original));
        // Polling again finds the same number: still this write, no echo.
        let again = refresh(&mut clipboard);
        assert!(again.latest.is_none());
        let received = read(&mut clipboard).unwrap();
        assert_eq!(received.text(), text);
        assert_eq!(received.origin(), Some(original));
        assert_eq!(format!("{received:?}"), "WindowsClipboardText([redacted])");
    }
}

#[test]
fn a_local_copy_of_the_same_text_is_a_new_change_without_origin() {
    let fake = Fake::default();
    let mut clipboard = owner(&fake, |_| true);
    clipboard.watch().unwrap();
    clipboard.prepare("same bytes", stamp(1)).unwrap();
    assert_eq!(
        clipboard.publish_until("same bytes", stamp(1), UNTIL),
        Publication::SubmittedToOs
    );
    let own = clipboard.changes().unwrap().latest.unwrap();
    fake.copy("same bytes");
    let local = refresh(&mut clipboard).latest.unwrap();
    assert!(local.revision > own.revision);
    assert_eq!(local.origin, None);
    clipboard.suspend().unwrap();
    clipboard.watch().unwrap();
    assert_eq!(read(&mut clipboard).unwrap().origin(), None);
}

#[test]
fn a_local_copy_after_prepare_is_kept_and_the_preparation_consumed() {
    let fake = Fake::default();
    let mut clipboard = owner(&fake, |_| true);
    clipboard.watch().unwrap();
    clipboard.prepare("remote", stamp(1)).unwrap();
    fake.copy("newer local");
    let writes = fake.calls();
    assert_eq!(
        clipboard.publish_until("remote", stamp(1), UNTIL),
        Publication::NotSubmitted(PlatformError::LocalChanged)
    );
    assert_eq!(fake.text().as_deref(), Some("newer local"));
    // Two metadata calls (the recheck), no write.
    assert_eq!(fake.calls(), writes + 2);
    assert_eq!(clipboard.changes().unwrap().latest.unwrap().origin, None);
    assert_eq!(read(&mut clipboard).unwrap().text(), "newer local");
    assert_eq!(
        clipboard.publish_until("remote", stamp(1), UNTIL),
        Publication::NotSubmitted(PlatformError::Unavailable)
    );
}

#[test]
fn a_copy_racing_the_open_is_refused_unwritten_and_reported_next() {
    let fake = Fake::default();
    let mut clipboard = owner(&fake, |_| true);
    clipboard.watch().unwrap();
    let revision = clipboard.revision();
    clipboard.prepare("remote", stamp(1)).unwrap();
    fake.0.borrow_mut().race = Some("raced");
    assert_eq!(
        clipboard.publish_until("remote", stamp(1), UNTIL),
        Publication::NotSubmitted(PlatformError::LocalChanged)
    );
    assert_eq!(fake.text().as_deref(), Some("raced"));
    assert!(!clipboard.retired);
    let change = clipboard.changes().unwrap().latest.unwrap();
    assert!(change.revision > revision);
    assert_eq!(change.origin, None);
}

#[test]
fn a_stale_read_is_consumed_and_the_newer_copy_reads_next() {
    let fake = Fake::default();
    let mut clipboard = owner(&fake, |_| true);
    clipboard.watch().unwrap();
    fake.copy("first");
    refresh(&mut clipboard);
    clipboard.begin_read().unwrap();
    fake.copy("second");
    assert_eq!(clipboard.poll_read().unwrap_err(), Error::LocalChanged);
    assert_eq!(clipboard.poll_read().unwrap_err(), Error::NotReading);
    assert!(clipboard.changes().unwrap().latest.is_some());
    assert_eq!(read(&mut clipboard).unwrap().text(), "second");
}

#[test]
fn oversize_invalid_and_absent_text_are_refused_without_text() {
    let fake = Fake::default();
    let mut clipboard = owner(&fake, |_| true);
    clipboard.watch().unwrap();
    assert_eq!(read(&mut clipboard).unwrap_err(), Error::NoText);
    let oversized = "x".repeat(clipboard.limit + 1);
    fake.copy(&oversized);
    refresh(&mut clipboard);
    assert_eq!(read(&mut clipboard).unwrap_err(), Error::Limit);
    assert_eq!(clipboard.poll_read().unwrap_err(), Error::NotReading);
    fake.copy("unpaired");
    fake.0.borrow_mut().invalid = true;
    refresh(&mut clipboard);
    assert_eq!(read(&mut clipboard).unwrap_err(), Error::Invalid);
    assert_eq!(
        clipboard.prepare(&oversized, stamp(1)),
        Err(PlatformError::Unsupported)
    );
}

#[test]
fn a_refusing_gate_at_publication_leaves_the_clipboard_and_the_owner() {
    let fake = Fake::default();
    fake.copy("local");
    let mut clipboard = owner(&fake, |until| until.is_none());
    clipboard.watch().unwrap();
    clipboard.prepare("remote", stamp(1)).unwrap();
    assert_eq!(
        clipboard.publish_until("remote", stamp(1), UNTIL),
        Publication::NotSubmitted(PlatformError::Unavailable)
    );
    assert_eq!(fake.text().as_deref(), Some("local"));
    assert!(clipboard.prepared.is_none());
    assert!(!clipboard.retired);
    assert_eq!(read(&mut clipboard).unwrap().text(), "local");
}

#[test]
fn a_gate_refusing_inside_the_open_writes_nothing() {
    // Allowed for the check before opening, refused at the check inside it.
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let fake = Fake::default();
    fake.copy("local");
    let mut clipboard = owner(&fake, move |until| {
        until.is_none() || counted.fetch_add(1, Ordering::AcqRel) < 1
    });
    clipboard.watch().unwrap();
    clipboard.prepare("remote", stamp(1)).unwrap();
    assert_eq!(
        clipboard.publish_until("remote", stamp(1), UNTIL),
        Publication::NotSubmitted(PlatformError::Unavailable)
    );
    assert_eq!(calls.load(Ordering::Acquire), 2);
    assert_eq!(fake.text().as_deref(), Some("local"));
    assert!(!clipboard.retired);
}

#[test]
fn expiry_after_the_write_is_unknown_and_fences_the_owner() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let fake = Fake::default();
    let mut clipboard = owner(&fake, move |until| {
        until.is_none() || counted.fetch_add(1, Ordering::AcqRel) < 2
    });
    clipboard.watch().unwrap();
    clipboard.prepare("remote", stamp(1)).unwrap();
    assert_eq!(
        clipboard.publish_until("remote", stamp(1), UNTIL),
        Publication::UnknownEffect
    );
    assert!(clipboard.retired);
    assert!(clipboard.prepared.is_none());
    assert!(clipboard.changes().is_err());
    assert_eq!(
        clipboard.publish_until("remote", stamp(1), UNTIL),
        Publication::NotSubmitted(PlatformError::Unavailable)
    );
}

#[test]
fn losing_ownership_before_the_number_is_read_is_unknown_never_owned() {
    let fake = Fake::default();
    let mut clipboard = owner(&fake, |_| true);
    clipboard.watch().unwrap();
    fake.0.borrow_mut().stolen = true;
    clipboard.prepare("remote", stamp(1)).unwrap();
    assert_eq!(
        clipboard.publish_until("remote", stamp(1), UNTIL),
        Publication::UnknownEffect
    );
    assert!(clipboard.retired);
    // The other program's copy never carries the controller's stamp.
    assert!(
        clipboard
            .revisions
            .current
            .is_none_or(|change| change.origin.is_none())
    );
}

#[test]
fn a_number_going_backwards_is_refused() {
    let fake = Fake::default();
    fake.copy("one");
    let mut clipboard = owner(&fake, |_| true);
    clipboard.watch().unwrap();
    fake.0.borrow_mut().sequence -= 1;
    clipboard.last_poll = None;
    assert_eq!(
        clipboard.changes().unwrap_err(),
        Error::Platform(PlatformError::Unavailable)
    );
}

#[test]
fn close_closes_the_window_once_and_never_touches_the_text() {
    let fake = Fake::default();
    fake.copy("local");
    let mut clipboard = owner(&fake, |_| true);
    clipboard.watch().unwrap();
    clipboard.prepare("remote", stamp(1)).unwrap();
    clipboard.close();
    assert_eq!(fake.0.borrow().closed, 1);
    assert!(clipboard.changes().is_err());
    drop(clipboard);
    assert_eq!(fake.0.borrow().closed, 1);
    assert_eq!(fake.text().as_deref(), Some("local"));
}
