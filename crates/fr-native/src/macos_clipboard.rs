//! The macOS plain UTF-8 pasteboard, confined to the original admitted
//! clipboard worker. No window, runtime, thread, native callback queue or grant
//! is created here. The visible application's gate remains live across every
//! native operation; publication additionally retains the original deadline.
use crate::macos_clipboard_state::{Change, Replacement, Revisions, refused};
use fr_core::{
    clipboard::{ClipboardSink, PlatformError, Publication, Stamp},
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_wire::clipboard::session::synchronize::{
    NativeChange, NativeChanges, NativeClipboard, NativeText,
};
use std::{
    ffi::c_void,
    fmt,
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::NonNull,
    rc::Rc,
    time::{Duration, Instant},
};

/// Original visible clipboard Off surface. A UI heartbeat grants no clipboard
/// or input authority; it only adds a terminal, bounded privacy prerequisite.
/// A late UI turn, clock regression or stop cannot re-enable this instance.
#[derive(Clone, Default)]
pub struct Privacy(crate::macos_clipboard_privacy::Heartbeat);

impl Privacy {
    /// Call only after the original native panel is verified visible.
    pub fn progress(&self) -> bool {
        self.0.progress()
    }

    /// A blocked UI becomes terminal after the existing 250 ms maximum silence.
    pub fn live(&self) -> bool {
        self.0.live()
    }

    pub fn stop(&self, user_off: bool) {
        self.0.stop(user_off);
    }

    pub fn stopped(&self) -> bool {
        self.0.stopped()
    }

    pub fn user_off(&self) -> bool {
        self.0.user_off()
    }
}

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const OK: i32 = 0;
const PERMISSION: i32 = -2;
const CHANGED: i32 = -3;
const NO_TEXT: i32 = -4;
const LIMIT: i32 = -5;
const DENIED: i32 = -6;
type Gate = dyn Fn(Option<HostInstant>) -> bool + Send + Sync;
type Check = unsafe extern "C" fn(*mut c_void) -> i32;

unsafe extern "C" {
    fn fr_mac_clip_open(unique: i32, limit: usize, out: *mut *mut c_void) -> i32;
    fn fr_mac_clip_close(owner: *mut c_void);
    fn fr_mac_clip_observe(owner: *mut c_void, count: *mut i64, has_text: *mut i32) -> i32;
    fn fr_mac_clip_prepare(owner: *mut c_void, bytes: *const u8, len: usize) -> i32;
    fn fr_mac_clip_cancel(owner: *mut c_void);
    fn fr_mac_clip_read(
        owner: *mut c_void,
        expected: i64,
        bytes: *mut u8,
        capacity: usize,
        used: *mut usize,
        check: Check,
        context: *mut c_void,
    ) -> i32;
    fn fr_mac_clip_declare(
        owner: *mut c_void,
        expected: i64,
        count: *mut i64,
        attempted: *mut i32,
        check: Check,
        context: *mut c_void,
    ) -> i32;
    fn fr_mac_clip_publish(
        owner: *mut c_void,
        expected: i64,
        count: *mut i64,
        check: Check,
        context: *mut c_void,
    ) -> i32;
    #[cfg(test)]
    fn fr_mac_clip_test_replace(owner: *mut c_void, bytes: *const u8, len: usize) -> i32;
}

struct Checked<'a> {
    gate: &'a Gate,
    until: Option<HostInstant>,
}

/// C invokes this synchronously, never retains it, and catches all Objective-C
/// exceptions itself. No Rust unwind is allowed to cross the native boundary.
unsafe extern "C" fn check(context: *mut c_void) -> i32 {
    // SAFETY: each call below lends a stack Checked for exactly one synchronous
    // native call; the C shim never stores the context or invokes it elsewhere.
    let checked = unsafe { &*context.cast::<Checked<'_>>() };
    i32::from(catch_unwind(AssertUnwindSafe(|| (checked.gate)(checked.until))).unwrap_or(false))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Platform(PlatformError),
    Access,
    NotWatching,
    NotReading,
    Busy,
    LocalChanged,
    NoText,
    Limit,
    Allocation,
    InvalidUtf8,
}

impl Error {
    fn platform(self) -> PlatformError {
        match self {
            Self::LocalChanged => PlatformError::LocalChanged,
            Self::NoText | Self::Limit | Self::InvalidUtf8 => PlatformError::Unsupported,
            Self::Platform(error) => error,
            _ => PlatformError::Unavailable,
        }
    }
}

fn status(code: i32) -> Result<(), Error> {
    match code {
        OK => Ok(()),
        PERMISSION => Err(Error::Platform(PlatformError::Permission)),
        CHANGED => Err(Error::LocalChanged),
        NO_TEXT => Err(Error::NoText),
        LIMIT => Err(Error::Limit),
        DENIED => Err(Error::Access),
        _ => Err(Error::Platform(PlatformError::Unavailable)),
    }
}

struct Bytes(Vec<u8>);

impl Drop for Bytes {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            // SAFETY: exclusive live byte references; volatile stores keep the
            // private-buffer erasure observable to the compiler before free.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

/// Complete UTF-8 only, never a borrowed native pointer or a partial selection.
pub struct Text {
    bytes: Bytes,
    origin: Option<Stamp>,
}

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MacClipboardText([redacted])")
    }
}

impl NativeText for Text {
    fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes.0).expect("validated complete UTF-8")
    }

    fn origin(&self) -> Option<Stamp> {
        self.origin
    }
}

struct Prepared {
    stamp: Stamp,
    change: Change,
    len: usize,
}

/// One original worker, one bounded pending read and one prepared publication.
/// `NSPasteboard` server/provider work can block; the containing worker retains
/// real pending cleanup and never claims cancellation killed a native call.
// Metadata settlement, subscription, terminal fencing and private test-board
// selection are independent facts, rather than mutually exclusive states.
#[allow(clippy::struct_excessive_bools)]
pub struct MacClipboard {
    handle: Option<NonNull<c_void>>,
    gate: Box<Gate>,
    limit: usize,
    revisions: Revisions,
    last_poll: Option<Instant>,
    settled: bool,
    watching: bool,
    reading: Option<Change>,
    prepared: Option<Prepared>,
    retired: bool,
    unique: bool,
    _thread: PhantomData<Rc<()>>,
}

impl fmt::Debug for MacClipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacClipboard")
            .field("open", &self.handle.is_some())
            .field("watching", &self.watching)
            .finish_non_exhaustive()
    }
}

impl MacClipboard {
    /// Called only by the admitted worker's factory, after the original visible
    /// local switch exists. Construction performs no native access; the board is
    /// acquired lazily on the first admitted turn, so a paused initial view need
    /// not fail setup. The gate combines that switch/privacy heartbeat and
    /// `frd::clipboard_quic::NativeAccess::check`; it must be nonblocking and
    /// must never recurse into this clipboard. `None` is observation; every
    /// mutation supplies the immutable received item's `Some(until)`.
    pub fn open(
        limits: &ProtocolLimits,
        gate: impl Fn(Option<HostInstant>) -> bool + Send + Sync + 'static,
    ) -> Result<Self, PlatformError> {
        Self::open_board(limits, Box::new(gate), false)
    }

    fn open_board(
        limits: &ProtocolLimits,
        gate: Box<Gate>,
        unique: bool,
    ) -> Result<Self, PlatformError> {
        let limit = limits.max_clipboard_item_bytes() as usize;
        if limit == 0 {
            return Err(PlatformError::Unsupported);
        }
        Ok(Self {
            handle: None,
            gate,
            limit,
            revisions: Revisions::default(),
            last_poll: None,
            settled: false,
            watching: false,
            reading: None,
            prepared: None,
            retired: false,
            unique,
            _thread: PhantomData,
        })
    }

    fn allowed(&self, until: Option<HostInstant>) -> bool {
        !self.retired && (self.gate)(until)
    }

    fn acquire(&mut self) -> Result<*mut c_void, Error> {
        if !self.allowed(None) {
            return Err(Error::Access);
        }
        if self.handle.is_none() {
            let mut raw = std::ptr::null_mut();
            // SAFETY: writable output, no borrowed data or callback retained.
            // Production requests only the general board after its live gate.
            status(unsafe { fr_mac_clip_open(i32::from(self.unique), self.limit, &raw mut raw) })?;
            self.handle =
                Some(NonNull::new(raw).ok_or(Error::Platform(PlatformError::Unavailable))?);
        }
        if !self.allowed(None) {
            return Err(Error::Access);
        }
        self.raw()
    }

    fn raw(&self) -> Result<*mut c_void, Error> {
        self.handle
            .map(NonNull::as_ptr)
            .ok_or(Error::Platform(PlatformError::Unavailable))
    }

    fn sample(&mut self) -> Result<Change, Error> {
        if !self.allowed(None) {
            return Err(Error::Access);
        }
        let mut count = 0;
        let mut has_text = 0;
        let raw = self.acquire()?;
        // SAFETY: live thread-confined native owner and writable scalar outputs.
        // This asks only for change/type metadata, never clipboard contents.
        let code = unsafe { fr_mac_clip_observe(raw, &raw mut count, &raw mut has_text) };
        self.last_poll = Some(Instant::now());
        self.settled = false;
        status(code)?;
        if !self.allowed(None) {
            return Err(Error::Access);
        }
        let change = self
            .revisions
            .observe(count, has_text != 0)
            .map_err(Error::Platform)?;
        self.settled = true;
        Ok(change)
    }

    fn prepare_at(
        &mut self,
        text: &str,
        stamp: Stamp,
        expected: Option<u64>,
    ) -> Result<(), PlatformError> {
        self.cancel_prepared();
        let result = self.prepare_inner(text, stamp, expected);
        if result.is_err() {
            self.cancel_prepared();
        }
        result
    }

    fn prepare_inner(
        &mut self,
        text: &str,
        stamp: Stamp,
        expected: Option<u64>,
    ) -> Result<(), PlatformError> {
        if !stamp.valid() || text.len() > self.limit {
            return Err(PlatformError::Unsupported);
        }
        let change = self.sample().map_err(Error::platform)?;
        if expected.is_some_and(|revision| revision != change.revision) {
            return Err(PlatformError::LocalChanged);
        }
        let raw = self.raw().map_err(Error::platform)?;
        // SAFETY: the shim copies the bounded borrowed bytes into one private
        // zeroed-on-cancel buffer; it does not publish or retain the Rust slice.
        status(unsafe { fr_mac_clip_prepare(raw, text.as_ptr(), text.len()) })
            .map_err(Error::platform)?;
        if self.sample().map_err(Error::platform)?.revision != change.revision {
            return Err(PlatformError::LocalChanged);
        }
        self.prepared = Some(Prepared {
            stamp,
            change,
            len: text.len(),
        });
        Ok(())
    }

    /// Consume the original prepared bytes once, without another text copy.
    /// The host's private IPC uses this only after its Prepare acknowledgement.
    /// `until` belongs to the clock used by this owner's original gate; the
    /// process adapter rounds its raw monotonic nanosecond cap DOWN to micros.
    pub fn publish_prepared_until(&mut self, stamp: Stamp, until: HostInstant) -> Publication {
        self.finish_publish(None, stamp, until)
    }

    fn finish_publish(
        &mut self,
        length: Option<usize>,
        stamp: Stamp,
        until: HostInstant,
    ) -> Publication {
        let result = self.publish_inner(length, stamp, until);
        self.cancel_prepared();
        if result == Publication::UnknownEffect {
            // Preserve the receipt and fence this owner before any later turn.
            // Native cleanup remains the original worker's responsibility.
            self.retired = true;
        }
        result
    }

    fn publish_inner(
        &mut self,
        length: Option<usize>,
        stamp: Stamp,
        until: HostInstant,
    ) -> Publication {
        self.reading = None;
        let Some(prepared) = self.prepared.take() else {
            return Publication::NotSubmitted(PlatformError::Unavailable);
        };
        if stamp != prepared.stamp || length.is_some_and(|len| len != prepared.len) {
            return Publication::NotSubmitted(PlatformError::Unavailable);
        }
        let replacement = match Replacement::new(prepared.change.count) {
            Ok(replacement) => replacement,
            Err(error) => return Publication::NotSubmitted(error),
        };
        let current = match self.sample() {
            Ok(current) => current,
            Err(error) => return Publication::NotSubmitted(error.platform()),
        };
        if current.revision != prepared.change.revision {
            return Publication::NotSubmitted(PlatformError::LocalChanged);
        }
        if !self.allowed(Some(until)) {
            return Publication::NotSubmitted(PlatformError::Unavailable);
        }
        let Ok(raw) = self.raw() else {
            return Publication::NotSubmitted(PlatformError::Unavailable);
        };
        let mut checked = Checked {
            gate: &*self.gate,
            until: Some(until),
        };
        let mut count = 0;
        let mut attempted = 0;
        // SAFETY: original handle, writable outputs and stack callback context
        // remain live until the synchronous call returns. No callback is queued.
        let code = unsafe {
            fr_mac_clip_declare(
                raw,
                current.count,
                &raw mut count,
                &raw mut attempted,
                check,
                (&raw mut checked).cast(),
            )
        };
        if let Err(error) = status(code) {
            return refused(attempted != 0, error.platform());
        }
        if !replacement.declared(count) || !self.allowed(Some(until)) {
            return Publication::UnknownEffect;
        }
        // SAFETY: same synchronous owner/context as above. The shim checks the
        // original deadline after preparation/ownership queries and immediately
        // before/after setData. No effect is retried or restored on uncertainty.
        let code = unsafe {
            fr_mac_clip_publish(raw, count, &raw mut count, check, (&raw mut checked).cast())
        };
        let publication = replacement.finish(code == OK, count, self.allowed(Some(until)));
        if publication == Publication::SubmittedToOs {
            if self.revisions.published(count, stamp).is_err() {
                return Publication::UnknownEffect;
            }
            self.settled = true;
            self.last_poll = Some(Instant::now());
        }
        publication
    }
}

impl ClipboardSink for MacClipboard {
    fn prepare(&mut self, text: &str, stamp: Stamp) -> Result<(), PlatformError> {
        self.prepare_at(text, stamp, None)
    }

    fn publish(&mut self, _text: &str, _stamp: Stamp) -> Publication {
        // This backend requires the real deadline; the core uses publish_until.
        self.cancel_read();
        self.cancel_prepared();
        Publication::NotSubmitted(PlatformError::Unavailable)
    }

    fn publish_until(&mut self, text: &str, stamp: Stamp, until: HostInstant) -> Publication {
        self.finish_publish(Some(text.len()), stamp, until)
    }

    fn cancel_prepared(&mut self) {
        self.prepared = None;
        if let Some(handle) = self.handle {
            // SAFETY: unique native owner; only private bytes are erased. This
            // does not clear or restore any operating-system pasteboard item.
            unsafe { fr_mac_clip_cancel(handle.as_ptr()) };
        }
    }
}

impl NativeClipboard for MacClipboard {
    type Text = Text;
    type Error = Error;

    fn watch(&mut self) -> Result<u64, Self::Error> {
        let change = self.sample()?;
        self.revisions.bootstrap();
        self.watching = true;
        Ok(change.revision)
    }

    fn changes(&mut self) -> Result<NativeChanges, Self::Error> {
        if self.retired {
            return Err(Error::Platform(PlatformError::Unavailable));
        }
        if !self.watching {
            return Err(Error::NotWatching);
        }
        // A temporary unusable view can suspend the original grant while the
        // synchronizer services metadata. Defer without touching the board; an
        // original terminal switch/lease remains the containing owner's fence.
        if !self.allowed(None) {
            return Ok(NativeChanges {
                latest: None,
                settled: false,
            });
        }
        if self
            .last_poll
            .is_none_or(|last| last.elapsed() >= POLL_INTERVAL)
        {
            match self.sample() {
                Ok(_) => {}
                Err(Error::Access | Error::LocalChanged) => {
                    return Ok(NativeChanges {
                        latest: None,
                        settled: false,
                    });
                }
                Err(error) => return Err(error),
            }
        }
        Ok(NativeChanges {
            latest: self.revisions.take_pending().map(|change| NativeChange {
                revision: change.revision,
                has_selection: change.has_text,
                origin: change.origin,
            }),
            settled: self.settled,
        })
    }

    fn revision(&self) -> u64 {
        self.revisions.revision()
    }

    fn prepare_for_revision(
        &mut self,
        text: &str,
        stamp: Stamp,
        revision: u64,
    ) -> Result<(), PlatformError> {
        self.prepare_at(text, stamp, Some(revision))
    }

    fn begin_read(&mut self) -> Result<(), Self::Error> {
        if !self.watching {
            return Err(Error::NotWatching);
        }
        if self.reading.is_some() {
            return Err(Error::Busy);
        }
        let revision = self.revision();
        let change = self.sample()?;
        if revision != change.revision {
            return Err(Error::LocalChanged);
        }
        if !change.has_text {
            return Err(Error::NoText);
        }
        self.reading = Some(change);
        Ok(())
    }

    fn poll_read(&mut self) -> Result<Option<Self::Text>, Self::Error> {
        let reading = self.reading.take().ok_or(Error::NotReading)?;
        if self.sample()?.revision != reading.revision {
            return Err(Error::LocalChanged);
        }
        let mut bytes = Bytes(Vec::new());
        bytes
            .0
            .try_reserve_exact(self.limit)
            .map_err(|_| Error::Allocation)?;
        bytes.0.resize(self.limit, 0);
        let mut used = 0;
        let raw = self.raw()?;
        let mut checked = Checked {
            gate: &*self.gate,
            until: None,
        };
        // SAFETY: writable initialized buffer of exactly capacity bytes; the
        // shim validates the NSData length before copying and never retains it.
        // Foundation may materialize provider data internally before that bound.
        let code = unsafe {
            fr_mac_clip_read(
                raw,
                reading.count,
                bytes.0.as_mut_ptr(),
                bytes.0.len(),
                &raw mut used,
                check,
                (&raw mut checked).cast(),
            )
        };
        // Even a refused native read can discover a newer local copy. Retain its
        // metadata for the next turn before consuming this old attempt forever.
        let after = self.sample()?;
        if after.revision != reading.revision {
            return Err(Error::LocalChanged);
        }
        status(code)?;
        if used > bytes.0.len() {
            return Err(Error::Limit);
        }
        bytes.0.truncate(used);
        std::str::from_utf8(&bytes.0).map_err(|_| Error::InvalidUtf8)?;
        Ok(Some(Text {
            bytes,
            origin: after.origin,
        }))
    }

    fn cancel_read(&mut self) {
        self.reading = None;
    }

    fn suspend(&mut self) -> Result<(), Self::Error> {
        self.cancel_read();
        self.cancel_prepared();
        self.watching = false;
        self.revisions.suspend();
        self.last_poll = None;
        self.settled = false;
        Ok(())
    }

    fn close(&mut self) {
        self.retired = true;
        self.cancel_read();
        self.prepared = None;
        self.watching = false;
        self.revisions.suspend();
        if let Some(handle) = self.handle.take() {
            // SAFETY: this thread owns the sole retained handle. Close erases
            // private buffers and releases it, without clearing the general board.
            unsafe { fr_mac_clip_close(handle.as_ptr()) };
        }
    }
}

impl Drop for MacClipboard {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::clipboard::Endpoint;
    use std::sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    /// One private board at a time in this process. Run on parallel test
    /// threads, these tests intermittently saw changes they did not make on
    /// their own unique boards (`LocalChanged`, `NoText`, `UnknownEffect`): 5 of 400
    /// runs on macOS 27, none of 400 run serially. Production opens only the
    /// general board, from its one owner thread.
    static NATIVE: Mutex<()> = Mutex::new(());

    /// The board drops before the lock is released.
    struct Board {
        owner: MacClipboard,
        _serial: MutexGuard<'static, ()>,
    }

    impl core::ops::Deref for Board {
        type Target = MacClipboard;
        fn deref(&self) -> &MacClipboard {
            &self.owner
        }
    }

    impl core::ops::DerefMut for Board {
        fn deref_mut(&mut self) -> &mut MacClipboard {
            &mut self.owner
        }
    }

    fn stamp(sequence: u64) -> Stamp {
        Stamp {
            id: u128::from(sequence),
            source: Endpoint::Host,
            sequence,
        }
    }

    fn board(gate: impl Fn(Option<HostInstant>) -> bool + Send + Sync + 'static) -> Board {
        let serial = NATIVE.lock().unwrap_or_else(PoisonError::into_inner);
        Board {
            owner: MacClipboard::open_board(&ProtocolLimits::ABSOLUTE, Box::new(gate), true)
                .unwrap(),
            _serial: serial,
        }
    }

    fn copy(owner: &mut MacClipboard, bytes: &[u8]) {
        let raw = owner.acquire().unwrap();
        // SAFETY: this test created a private board; the test-only native entry
        // refuses a general board and copies the borrowed bytes synchronously.
        let status = unsafe { fr_mac_clip_test_replace(raw, bytes.as_ptr(), bytes.len()) };
        assert_eq!(status, OK);
    }

    fn refresh(owner: &mut MacClipboard) -> NativeChanges {
        owner.last_poll = None;
        owner.changes().unwrap()
    }

    #[test]
    fn paused_factory_has_no_native_access_and_resume_watches_once() {
        let enabled = Arc::new(AtomicBool::new(false));
        let gate = enabled.clone();
        let mut owner = board(move |_| gate.load(Ordering::Acquire));
        assert!(owner.handle.is_none());
        assert_eq!(owner.watch(), Err(Error::Access));
        assert!(owner.handle.is_none());
        enabled.store(true, Ordering::Release);
        assert!(owner.watch().unwrap() > 0);
        assert!(owner.handle.is_some());
        enabled.store(false, Ordering::Release);
        let paused = owner.changes().unwrap();
        assert!(paused.latest.is_none());
        assert!(!paused.settled);
        enabled.store(true, Ordering::Release);
        assert!(refresh(&mut owner).settled);
    }

    #[test]
    fn unicode_nul_and_empty_text_use_complete_utf8_and_exact_origin() {
        let mut owner = board(|_| true);
        owner.watch().unwrap();
        for (sequence, text) in [(1, "λ\0\n🦀"), (2, "")] {
            let original = stamp(sequence);
            let revision = owner.revision();
            owner
                .prepare_for_revision(text, original, revision)
                .unwrap();
            assert_eq!(
                owner.publish_until(text, original, HostInstant::from_micros(100)),
                Publication::SubmittedToOs
            );
            let change = owner.changes().unwrap().latest.unwrap();
            assert!(change.has_selection);
            assert_eq!(change.origin, Some(original));
            owner.begin_read().unwrap();
            let received = owner.poll_read().unwrap().unwrap();
            assert_eq!(received.text(), text);
            assert_eq!(received.origin(), Some(original));
            assert_eq!(format!("{received:?}"), "MacClipboardText([redacted])");
        }
    }

    #[test]
    fn external_same_text_is_new_copy_and_survives_suspend() {
        let mut owner = board(|_| true);
        owner.watch().unwrap();
        let text = "same bytes";
        owner.prepare(text, stamp(1)).unwrap();
        assert_eq!(
            owner.publish_until(text, stamp(1), HostInstant::from_micros(100)),
            Publication::SubmittedToOs
        );
        let own = owner.changes().unwrap().latest.unwrap();
        copy(&mut owner, text.as_bytes());
        let external = refresh(&mut owner).latest.unwrap();
        assert!(external.revision > own.revision);
        assert_eq!(external.origin, None);
        owner.suspend().unwrap();
        owner.watch().unwrap();
        owner.begin_read().unwrap();
        assert_eq!(owner.poll_read().unwrap().unwrap().text(), text);
    }

    #[test]
    fn local_copy_during_preparation_is_retained_without_overwrite_or_retry() {
        let mut owner = board(|_| true);
        owner.watch().unwrap();
        owner.prepare("remote", stamp(1)).unwrap();
        copy(&mut owner, b"new local selection");
        assert_eq!(
            owner.publish_until("remote", stamp(1), HostInstant::from_micros(100)),
            Publication::NotSubmitted(PlatformError::LocalChanged)
        );
        assert!(owner.prepared.is_none());
        assert!(owner.reading.is_none());
        let change = owner.changes().unwrap().latest.unwrap();
        assert_eq!(change.origin, None);
        owner.begin_read().unwrap();
        assert_eq!(
            owner.poll_read().unwrap().unwrap().text(),
            "new local selection"
        );
        assert_eq!(
            owner.publish_until("remote", stamp(1), HostInstant::from_micros(200)),
            Publication::NotSubmitted(PlatformError::Unavailable)
        );
    }

    #[test]
    fn local_copy_after_begin_read_consumes_stale_read_and_keeps_new_metadata() {
        let mut owner = board(|_| true);
        owner.watch().unwrap();
        copy(&mut owner, b"first");
        refresh(&mut owner);
        owner.begin_read().unwrap();
        copy(&mut owner, b"second");
        assert_eq!(owner.poll_read().unwrap_err(), Error::LocalChanged);
        assert_eq!(owner.poll_read().unwrap_err(), Error::NotReading);
        assert!(owner.changes().unwrap().latest.is_some());
        owner.begin_read().unwrap();
        assert_eq!(owner.poll_read().unwrap().unwrap().text(), "second");
    }

    #[test]
    fn native_invalid_utf8_and_oversize_are_consumed_without_partial_read() {
        let mut owner = board(|_| true);
        owner.watch().unwrap();
        copy(&mut owner, &[0xff]);
        refresh(&mut owner);
        owner.begin_read().unwrap();
        assert_eq!(owner.poll_read().unwrap_err(), Error::InvalidUtf8);
        assert_eq!(owner.poll_read().unwrap_err(), Error::NotReading);
        let oversized = vec![b'x'; owner.limit + 1];
        copy(&mut owner, &oversized);
        refresh(&mut owner);
        owner.begin_read().unwrap();
        assert_eq!(owner.poll_read().unwrap_err(), Error::Limit);
        assert_eq!(owner.poll_read().unwrap_err(), Error::NotReading);
    }

    #[test]
    fn original_gate_refusal_before_mutation_leaves_selection_and_consumes_prepare() {
        let mut owner = board(|until| until.is_none());
        owner.watch().unwrap();
        copy(&mut owner, b"local");
        refresh(&mut owner);
        owner.prepare("remote", stamp(1)).unwrap();
        assert_eq!(
            owner.publish_until("remote", stamp(1), HostInstant::from_micros(100)),
            Publication::NotSubmitted(PlatformError::Unavailable)
        );
        owner.begin_read().unwrap();
        assert_eq!(owner.poll_read().unwrap().unwrap().text(), "local");
        assert!(owner.prepared.is_none());
        assert!(!owner.retired);
    }

    #[test]
    fn expiry_after_declaration_is_unknown_and_cannot_echo_or_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = calls.clone();
        let mut owner =
            board(move |until| until.is_none() || gate.fetch_add(1, Ordering::AcqRel) < 3);
        owner.watch().unwrap();
        owner.prepare("remote", stamp(1)).unwrap();
        assert_eq!(
            owner.publish_until("remote", stamp(1), HostInstant::from_micros(100)),
            Publication::UnknownEffect
        );
        assert!(owner.retired);
        assert!(owner.prepared.is_none());
        assert!(owner.reading.is_none());
        assert!(owner.changes().is_err());
        assert_eq!(
            owner.publish_until("remote", stamp(1), HostInstant::from_micros(200)),
            Publication::NotSubmitted(PlatformError::Unavailable)
        );
    }
    #[test]
    fn prepared_process_publication_uses_original_bytes_once_and_keeps_its_stamp() {
        let mut owner = board(|_| true);
        owner.watch().unwrap();
        let original = stamp(1);
        let revision = owner.revision();
        owner
            .prepare_for_revision("host process λ", original, revision)
            .unwrap();
        assert_eq!(
            owner.publish_prepared_until(original, HostInstant::from_micros(100)),
            Publication::SubmittedToOs
        );
        let after = owner.revision();
        assert_eq!(
            owner.publish_prepared_until(original, HostInstant::from_micros(200)),
            Publication::NotSubmitted(PlatformError::Unavailable)
        );
        assert_eq!(owner.revision(), after);
        owner.begin_read().unwrap();
        let text = owner.poll_read().unwrap().unwrap();
        assert_eq!(text.text(), "host process λ");
        assert_eq!(text.origin(), Some(original));
    }

    #[test]
    fn prepared_process_publication_cannot_replace_a_newer_local_copy() {
        let mut owner = board(|_| true);
        owner.watch().unwrap();
        owner.prepare("remote", stamp(1)).unwrap();
        copy(&mut owner, b"newer local");
        assert_eq!(
            owner.publish_prepared_until(stamp(1), HostInstant::from_micros(100)),
            Publication::NotSubmitted(PlatformError::LocalChanged)
        );
        owner.begin_read().unwrap();
        assert_eq!(owner.poll_read().unwrap().unwrap().text(), "newer local");
    }
}
