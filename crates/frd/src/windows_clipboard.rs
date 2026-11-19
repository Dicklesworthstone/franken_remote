//! The Windows text clipboard on either end of a controlled session: a host's
//! for its controller (`frd run --control`), or a controlling viewer's own
//! (fr-windows `--control`, through fr-mobile-ffi's session). frd owns it
//! itself, on the clipboard worker thread, through
//! `fr_winsys::clipboard`: Win32's clipboard loads no library frd must keep
//! out of process, so there is no helper child. The rules are the macOS
//! owner's (`fr_native::macos_clipboard`): change metadata (the window
//! station's sequence number and whether Unicode text is offered) is polled
//! at most every 50 ms without opening the clipboard; a read and a write each
//! recheck that number with the clipboard open, so a newer local copy is
//! never overwritten or misattributed; and the number after a write carries
//! its stamp, so the synchronizer does not send the controller's own text
//! back as a new copy.
//!
//! Privacy: a "Text clipboard sharing is on" window with a Turn off button
//! (`fr_winsys::indicator::Kind::Clipboard`) is on screen and answering
//! before the first native access, and stays for this owner's life. Turn off,
//! closing it, or its UI thread not answering within 250 ms stops clipboard
//! sharing for this connection (the original `native_clipboard::Control`);
//! every native call also requires an answer within the last 300 ms. The
//! window is the clipboard owner a write names.
use crate::clipboard_quic::NativeOpenError;
use fr_core::{
    clipboard::{ClipboardSink, PlatformError, Publication, Stamp},
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_wire::clipboard::session::synchronize::{
    NativeChange, NativeChanges, NativeClipboard, NativeText,
};
use std::{
    fmt,
    marker::PhantomData,
    rc::Rc,
    time::{Duration, Instant},
};

const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Why a native clipboard call did not complete (`fr_winsys::clipboard`'s
/// errors, available on every platform for the tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Another program held the clipboard open for the whole wait.
    Busy,
    /// The sequence number was not the expected one once open; after a
    /// write, another program emptied the clipboard before its number was read.
    Changed,
    NoText,
    Limit,
    /// Not valid UTF-16.
    Invalid,
    /// The caller's check, run while open, refused.
    Refused,
    Unavailable,
    /// The native privacy owner exited without successful cleanup.
    Cleanup,
}

/// What the owner asks of the clipboard; `System` is the real one.
pub trait Native {
    type Text: AsRef<str>;
    /// The change number; no open needed.
    fn sequence(&mut self) -> u32;
    fn has_text(&mut self) -> bool;
    /// The Unicode text, at most `limit` UTF-8 bytes, if the number is still
    /// `expected` once open and `allowed()` passes there.
    fn read(
        &mut self,
        limit: usize,
        expected: u32,
        allowed: &dyn Fn() -> bool,
    ) -> Result<Self::Text, Failure>;
    /// Replace the text if the number is still `expected` once open and
    /// `allowed()` passes there; the number after the write, or the failure
    /// and whether the clipboard was already emptied.
    fn write(
        &mut self,
        text: &str,
        expected: u32,
        allowed: &dyn Fn() -> bool,
    ) -> Result<u32, (Failure, bool)>;
    /// Request closure of the privacy window once. Never clears the clipboard.
    fn close(&mut self);
    /// Nonblocking inspection of the original privacy owner's cleanup. Pending
    /// work must retain its thread/resources; only synchronous adapters use the
    /// default. Completion (including an error) must remain observable.
    fn poll_close(&mut self) -> Option<Result<(), Failure>> {
        Some(Ok(()))
    }
}

type Gate = dyn Fn(Option<HostInstant>) -> bool + Send + Sync;

#[derive(Clone, Copy)]
struct Change {
    revision: u64,
    count: u32,
    has_text: bool,
    origin: Option<Stamp>,
}

/// The macOS owner's revision rules over the sequence number: wire revisions
/// are positive and monotonic, a number going backwards is a refusal, and only
/// this owner's own confirmed write carries provenance.
#[derive(Default)]
struct Revisions {
    current: Option<Change>,
    pending: bool,
}

impl Revisions {
    fn revision(&self) -> u64 {
        self.current.map_or(0, |change| change.revision)
    }

    fn observe(&mut self, count: u32, has_text: bool) -> Result<Change, PlatformError> {
        if self.current.is_some_and(|old| count < old.count) {
            return Err(PlatformError::Unavailable);
        }
        if let Some(old) = self
            .current
            .filter(|old| old.count == count && old.has_text == has_text)
        {
            return Ok(old);
        }
        let revision = self
            .revision()
            .checked_add(1)
            .ok_or(PlatformError::Unavailable)?;
        let change = Change {
            revision,
            count,
            has_text,
            origin: None,
        };
        self.current = Some(change);
        self.pending = true;
        Ok(change)
    }

    fn bootstrap(&mut self) {
        self.pending = self.current.is_some();
    }

    fn take_pending(&mut self) -> Option<Change> {
        if std::mem::take(&mut self.pending) {
            self.current
        } else {
            None
        }
    }

    fn published(&mut self, count: u32, stamp: Stamp) -> Result<(), PlatformError> {
        let mut change = self.observe(count, true)?;
        change.origin = Some(stamp);
        self.current = Some(change);
        self.pending = true;
        Ok(())
    }

    fn suspend(&mut self) {
        if let Some(change) = self.current.as_mut() {
            change.origin = None;
        }
        self.pending = false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Platform(PlatformError),
    /// The privacy window or the original authority refused.
    Access,
    NotWatching,
    NotReading,
    /// A read is already pending.
    Busy,
    LocalChanged,
    NoText,
    Limit,
    Allocation,
    Invalid,
    Cleanup,
}

impl Error {
    fn platform(self) -> PlatformError {
        match self {
            Self::LocalChanged => PlatformError::LocalChanged,
            Self::NoText | Self::Limit | Self::Invalid => PlatformError::Unsupported,
            Self::Platform(error) => error,
            _ => PlatformError::Unavailable,
        }
    }

    fn native(failure: Failure) -> Self {
        match failure {
            Failure::Changed => Self::LocalChanged,
            Failure::NoText => Self::NoText,
            Failure::Limit => Self::Limit,
            Failure::Invalid => Self::Invalid,
            Failure::Refused => Self::Access,
            Failure::Cleanup => Self::Cleanup,
            Failure::Busy | Failure::Unavailable => Self::Platform(PlatformError::Unavailable),
        }
    }
}

/// Complete UTF-8 read from the clipboard; zeroed on drop by the native text.
pub struct Text<T> {
    text: T,
    origin: Option<Stamp>,
}

impl<T> fmt::Debug for Text<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WindowsClipboardText([redacted])")
    }
}

impl<T: AsRef<str>> NativeText for Text<T> {
    fn text(&self) -> &str {
        self.text.as_ref()
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

/// One owner on the clipboard worker thread: one pending read and one
/// prepared publication at a time.
// Metadata settlement, subscription and the terminal fence are independent.
#[allow(clippy::struct_excessive_bools)]
pub struct Clipboard<N: Native> {
    native: N,
    gate: Box<Gate>,
    limit: usize,
    revisions: Revisions,
    last_poll: Option<Instant>,
    settled: bool,
    watching: bool,
    reading: Option<Change>,
    prepared: Option<Prepared>,
    retired: bool,
    closed: bool,
    _thread: PhantomData<Rc<()>>,
}

impl<N: Native> fmt::Debug for Clipboard<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsClipboard")
            .field("watching", &self.watching)
            .field("retired", &self.retired)
            .finish_non_exhaustive()
    }
}

impl<N: Native> Clipboard<N> {
    /// No native access here: the first is the first admitted turn. `gate`
    /// combines the privacy window's liveness with the original worker's
    /// `NativeAccess::check`; it must not block or call back into this owner.
    /// `None` is observation; a write supplies its item's `Some(until)`.
    pub fn open(
        mut native: N,
        limits: &ProtocolLimits,
        gate: impl Fn(Option<HostInstant>) -> bool + Send + Sync + 'static,
    ) -> Result<Self, NativeOpenError> {
        let limit = limits.max_clipboard_item_bytes() as usize;
        if limit == 0 {
            native.close();
            loop {
                if let Some(result) = native.poll_close() {
                    return Err(if result.is_ok() {
                        NativeOpenError::Native(PlatformError::Unsupported)
                    } else {
                        NativeOpenError::Cleanup
                    });
                }
                std::thread::park_timeout(Duration::from_millis(5));
            }
        }
        Ok(Self {
            native,
            gate: Box::new(gate),
            limit,
            revisions: Revisions::default(),
            last_poll: None,
            settled: false,
            watching: false,
            reading: None,
            prepared: None,
            retired: false,
            closed: false,
            _thread: PhantomData,
        })
    }

    fn allowed(&self, until: Option<HostInstant>) -> bool {
        !self.retired && (self.gate)(until)
    }

    fn sample(&mut self) -> Result<Change, Error> {
        if !self.allowed(None) {
            return Err(Error::Access);
        }
        let count = self.native.sequence();
        let has_text = self.native.has_text();
        self.last_poll = Some(Instant::now());
        self.settled = false;
        if !self.allowed(None) {
            return Err(Error::Access);
        }
        let change = self
            .revisions
            .observe(count, has_text)
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
        self.prepared = None;
        if !stamp.valid() || text.len() > self.limit {
            return Err(PlatformError::Unsupported);
        }
        let change = self.sample().map_err(Error::platform)?;
        if expected.is_some_and(|revision| revision != change.revision) {
            return Err(PlatformError::LocalChanged);
        }
        self.prepared = Some(Prepared {
            stamp,
            change,
            len: text.len(),
        });
        Ok(())
    }

    fn finish_publish(&mut self, text: &str, stamp: Stamp, until: HostInstant) -> Publication {
        let result = self.publish_inner(text, stamp, until);
        self.prepared = None;
        if result == Publication::UnknownEffect {
            // Keep the receipt and fence this owner before any later turn.
            self.retired = true;
        }
        result
    }

    fn publish_inner(&mut self, text: &str, stamp: Stamp, until: HostInstant) -> Publication {
        self.reading = None;
        let Some(prepared) = self.prepared.take() else {
            return Publication::NotSubmitted(PlatformError::Unavailable);
        };
        if stamp != prepared.stamp || text.len() != prepared.len {
            return Publication::NotSubmitted(PlatformError::Unavailable);
        }
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
        let (gate, retired) = (&*self.gate, self.retired);
        let allowed = || !retired && gate(Some(until));
        match self.native.write(text, current.count, &allowed) {
            Ok(count) => {
                if !self.allowed(Some(until)) || self.revisions.published(count, stamp).is_err() {
                    return Publication::UnknownEffect;
                }
                self.settled = true;
                self.last_poll = Some(Instant::now());
                Publication::SubmittedToOs
            }
            Err((_, true)) => Publication::UnknownEffect,
            Err((Failure::Changed, false)) => {
                // Keep the newer local copy's metadata for the next turn.
                let _ = self.sample();
                Publication::NotSubmitted(PlatformError::LocalChanged)
            }
            Err((_, false)) => Publication::NotSubmitted(PlatformError::Unavailable),
        }
    }
}

impl<N: Native> ClipboardSink for Clipboard<N> {
    fn prepare(&mut self, text: &str, stamp: Stamp) -> Result<(), PlatformError> {
        self.prepare_at(text, stamp, None)
    }

    fn publish(&mut self, _text: &str, _stamp: Stamp) -> Publication {
        // Publication needs the item's deadline; the core uses publish_until.
        self.cancel_read();
        self.cancel_prepared();
        Publication::NotSubmitted(PlatformError::Unavailable)
    }

    fn publish_until(&mut self, text: &str, stamp: Stamp, until: HostInstant) -> Publication {
        self.finish_publish(text, stamp, until)
    }

    fn cancel_prepared(&mut self) {
        self.prepared = None;
    }
}

impl<N: Native> NativeClipboard for Clipboard<N> {
    type Text = Text<N::Text>;
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
        let deferred = NativeChanges {
            latest: None,
            settled: false,
        };
        if !self.allowed(None) {
            return Ok(deferred);
        }
        if self
            .last_poll
            .is_none_or(|last| last.elapsed() >= POLL_INTERVAL)
        {
            match self.sample() {
                Ok(_) => {}
                Err(Error::Access) => return Ok(deferred),
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
        let (gate, retired) = (&*self.gate, self.retired);
        let allowed = || !retired && gate(None);
        let text = self.native.read(self.limit, reading.count, &allowed);
        // Even a refused read can discover a newer local copy: keep its
        // metadata for the next turn before consuming this attempt.
        let after = self.sample()?;
        if after.revision != reading.revision {
            return Err(Error::LocalChanged);
        }
        let text = text.map_err(Error::native)?;
        if text.as_ref().len() > self.limit {
            return Err(Error::Limit);
        }
        Ok(Some(Text {
            text,
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
        if !std::mem::replace(&mut self.closed, true) {
            self.native.close();
        }
    }

    fn poll_close(&mut self) -> Option<Result<(), Self::Error>> {
        self.close();
        self.native
            .poll_close()
            .map(|result| result.map_err(Error::native))
    }
}

impl<N: Native> Drop for Clipboard<N> {
    fn drop(&mut self) {
        self.close();
    }
}

/// Original native supervisor custody, independent of the caller's reap wait.
/// Its result is retained; dropping an unfinished owner on the clipboard thread
/// waits for that same worker, never detaches it or creates a replacement.
#[cfg(any(windows, test))]
struct CleanupThread {
    worker: Option<std::thread::JoinHandle<bool>>,
    result: Option<Result<(), Failure>>,
}

#[cfg(any(windows, test))]
impl CleanupThread {
    fn new(worker: std::thread::JoinHandle<bool>) -> Self {
        Self {
            worker: Some(worker),
            result: None,
        }
    }

    fn poll(&mut self) -> Option<Result<(), Failure>> {
        if let Some(result) = self.result {
            return Some(result);
        }
        if !self.worker.as_ref()?.is_finished() {
            return None;
        }
        let result = match self.worker.take()?.join() {
            Ok(true) => Ok(()),
            Ok(false) | Err(_) => Err(Failure::Cleanup),
        };
        self.result = Some(result);
        Some(result)
    }
}

#[cfg(any(windows, test))]
impl Drop for CleanupThread {
    fn drop(&mut self) {
        while self.poll().is_none() {
            std::thread::park_timeout(Duration::from_millis(5));
        }
    }
}

#[cfg(windows)]
pub use system::{System, WindowsClipboard, factory};

#[cfg(windows)]
mod system {
    use super::{CleanupThread, Clipboard, Failure, Native};
    use crate::{
        clipboard_quic::{NativeAccess, NativeOpenError},
        native_clipboard::Control,
    };
    use fr_core::{clipboard::PlatformError, limits::ProtocolLimits};
    use fr_winsys::{
        clipboard as win32,
        indicator::{Indicator, Kind, OpenError},
    };
    use std::{
        sync::{
            Arc, Mutex, PoisonError,
            atomic::{AtomicBool, AtomicU64, Ordering},
            mpsc::{self, RecvTimeoutError},
        },
        thread,
        time::{Duration, Instant},
    };

    /// How long the window's UI thread may take to answer one check.
    const CHECK_BUDGET: Duration = Duration::from_millis(250);
    const CHECK_INTERVAL: Duration = Duration::from_millis(50);
    /// Native access needs an answer at most this old.
    const LIVE: Duration = Duration::from_millis(300);
    /// Another program holding the clipboard open (a clipboard manager
    /// reading a change) delays a read or write at most this long.
    const OPEN_WAIT: Duration = Duration::from_millis(200);

    pub type WindowsClipboard = Clipboard<System>;

    struct Shared {
        origin: Instant,
        /// Micros after `origin` of the last answer, plus one (0: none yet).
        answered: AtomicU64,
        off: AtomicBool,
    }

    impl Shared {
        fn answer(&self) {
            let micros = u64::try_from(self.origin.elapsed().as_micros()).unwrap_or(u64::MAX);
            self.answered
                .store(micros.saturating_add(1), Ordering::Release);
        }

        fn live(&self) -> bool {
            if self.off.load(Ordering::Acquire) {
                return false;
            }
            let answered = self.answered.load(Ordering::Acquire);
            answered != 0
                && self
                    .origin
                    .elapsed()
                    .saturating_sub(Duration::from_micros(answered - 1))
                    <= LIVE
        }
    }

    /// The clipboard window and its supervising thread.
    pub struct System {
        shared: Arc<Shared>,
        window: isize,
        stop: Option<mpsc::Sender<()>>,
        cleanup: CleanupThread,
    }

    impl System {
        fn open(control: &Control) -> Result<Self, NativeOpenError> {
            let shared = Arc::new(Shared {
                origin: Instant::now(),
                answered: AtomicU64::new(0),
                off: AtomicBool::new(false),
            });
            let (turned_off, original) = (shared.clone(), control.clone());
            let indicator = Indicator::open_kind_retained(Kind::Clipboard, move || {
                turned_off.off.store(true, Ordering::Release);
                original.stop();
            })
            .map_err(|error| match error {
                OpenError::Setup(_) => NativeOpenError::Native(PlatformError::Unavailable),
                OpenError::Cleanup => NativeOpenError::Cleanup,
            })?;
            if !indicator.responsive(CHECK_BUDGET) {
                shared.off.store(true, Ordering::Release);
                control.stop();
                return Err(if indicator.finish_close() {
                    NativeOpenError::Native(PlatformError::Unavailable)
                } else {
                    NativeOpenError::Cleanup
                });
            }
            shared.answer();
            let window = indicator.window();
            let (stop, stopped) = mpsc::channel();
            let watched = shared.clone();
            // A failed spawn drops its closure. Keep the original indicator in
            // this one-use cell so its cleanup result is still collectable.
            let owner = Arc::new(Mutex::new(Some(indicator)));
            let taken = owner.clone();
            let supervising = control.clone();
            let worker = thread::Builder::new()
                .name("fr-clipboard-privacy".into())
                .spawn(move || {
                    let indicator = taken.lock().unwrap_or_else(PoisonError::into_inner).take();
                    indicator.is_some_and(|indicator| {
                        supervise(indicator, &watched, &supervising, &stopped)
                    })
                });
            let Ok(worker) = worker else {
                shared.off.store(true, Ordering::Release);
                control.stop();
                let indicator = owner.lock().unwrap_or_else(PoisonError::into_inner).take();
                return Err(if indicator.is_some_and(Indicator::finish_close) {
                    NativeOpenError::Native(PlatformError::Unavailable)
                } else {
                    NativeOpenError::Cleanup
                });
            };
            Ok(Self {
                shared,
                window,
                stop: Some(stop),
                cleanup: CleanupThread::new(worker),
            })
        }
    }

    /// Check the window every 50 ms until the owner closes it; a check it
    /// does not answer within 250 ms stops clipboard sharing.
    fn supervise(
        mut indicator: Indicator,
        shared: &Shared,
        control: &Control,
        stopped: &mpsc::Receiver<()>,
    ) -> bool {
        // A message or the owner dropping its sender ends the checks.
        while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(CHECK_INTERVAL) {
            if !indicator.responsive(CHECK_BUDGET) {
                shared.off.store(true, Ordering::Release);
                control.stop();
                break;
            }
            shared.answer();
        }
        shared.off.store(true, Ordering::Release);
        control.stop();
        indicator.begin_close();
        loop {
            if let Some(result) = indicator.poll_close() {
                return result;
            }
            thread::park_timeout(CHECK_INTERVAL);
        }
    }

    const fn failure(error: win32::Error) -> Failure {
        match error {
            win32::Error::Busy => Failure::Busy,
            win32::Error::Changed => Failure::Changed,
            win32::Error::NoText => Failure::NoText,
            win32::Error::Limit => Failure::Limit,
            win32::Error::Invalid => Failure::Invalid,
            win32::Error::Refused => Failure::Refused,
            win32::Error::Unavailable => Failure::Unavailable,
        }
    }

    impl Native for System {
        type Text = win32::Text;

        fn sequence(&mut self) -> u32 {
            win32::sequence()
        }

        fn has_text(&mut self) -> bool {
            win32::has_text()
        }

        fn read(
            &mut self,
            limit: usize,
            expected: u32,
            allowed: &dyn Fn() -> bool,
        ) -> Result<win32::Text, Failure> {
            win32::read_text(limit, expected, OPEN_WAIT, allowed).map_err(failure)
        }

        fn write(
            &mut self,
            text: &str,
            expected: u32,
            allowed: &dyn Fn() -> bool,
        ) -> Result<u32, (Failure, bool)> {
            win32::write_text(self.window, text, expected, OPEN_WAIT, allowed)
                .map_err(|error| (failure(error.error), error.attempted))
        }

        fn close(&mut self) {
            self.shared.off.store(true, Ordering::Release);
            drop(self.stop.take());
        }

        fn poll_close(&mut self) -> Option<Result<(), Failure>> {
            self.cleanup.poll()
        }
    }

    impl Drop for System {
        fn drop(&mut self) {
            self.close();
        }
    }

    /// The clipboard worker's native factory: the window first, then an owner
    /// gated on it and on the original worker's authority.
    pub fn factory()
    -> impl FnOnce(Control, NativeAccess) -> Result<WindowsClipboard, NativeOpenError> + Send + 'static
    {
        move |control, access| {
            let native = System::open(&control)?;
            let shared = native.shared.clone();
            Clipboard::open(native, &ProtocolLimits::ABSOLUTE, move |until| {
                shared.live() && access.check(until)
            })
        }
    }
}

#[cfg(test)]
mod tests;
