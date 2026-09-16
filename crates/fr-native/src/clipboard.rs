//! Opt-in Linux X11 text observation and publication through a narrow XCB boundary.
//!
//! This owner belongs on the interactive-session native worker, not a transport
//! task or an authority mutex. Its caller pumps bounded native events and drops
//! it on session teardown. Local reads are explicit, bounded observations.
//! Neither direction injects paste keys, grants control, starts a thread, or
//! installs a runtime. Reading does not authorize disclosure to a peer.
use fr_core::{
    clipboard::{ClipboardSink, PlatformError, Publication, Stamp},
    limits::ProtocolLimits,
};
use std::{
    ffi::CString,
    fmt,
    ptr::NonNull,
    rc::Rc,
    time::{Duration, Instant},
};
mod ffi;
mod read;
use ffi::{Atoms, Event};
pub use read::{ReadError, ReadText};

const READERS: usize = 4;
const CHUNK: usize = 16_384;
const READER_LIFETIME: Duration = Duration::from_secs(3);
const PREPARE_LIFETIME: Duration = Duration::from_millis(100);

struct Text(Vec<u8>);
impl Drop for Text {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}
struct Selection {
    text: Text,
    stamp: Stamp,
    time: u32,
}
struct Reader {
    window: u32,
    property: u32,
    selection: Rc<Selection>,
    offset: usize,
    deadline: Instant,
}

/// Thread-confined connection. The `Rc` fields deliberately prevent Send/Sync.
/// Up to four immutable old selections may complete INCR reads while a new
/// current selection is published; at most six admitted-size native text
/// buffers exist, including preparation, plus one bounded local-read buffer.
/// Native calls never retain Rust slices.
pub struct X11Clipboard {
    handle: Option<NonNull<core::ffi::c_void>>,
    atoms: Atoms,
    window: u32,
    limit: usize,
    prepared: Option<Rc<Selection>>,
    current: Option<Rc<Selection>>,
    readers: [Option<Reader>; READERS],
    capture: Option<read::Capture>,
}
impl fmt::Debug for X11Clipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("X11Clipboard")
            .field("closed", &self.handle.is_none())
            .field("active_readers", &self.active_readers())
            .finish_non_exhaustive()
    }
}
impl X11Clipboard {
    /// `granted` is a local OS-session capability, never a peer-supplied boolean.
    /// Only explicit local Unix X displays are accepted; no ambient DISPLAY or
    /// peer-controlled TCP display endpoint is inferred.
    pub fn open(
        display: &str,
        limits: &ProtocolLimits,
        granted: bool,
    ) -> Result<Self, PlatformError> {
        if !granted {
            return Err(PlatformError::Permission);
        }
        let display_number = display
            .strip_prefix(':')
            .ok_or(PlatformError::Unsupported)?;
        let mut pieces = display_number.split('.');
        let valid_number =
            |s: &str| !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit());
        if !pieces.next().is_some_and(valid_number)
            || pieces.next().is_some_and(|s| !valid_number(s))
            || pieces.next().is_some()
        {
            return Err(PlatformError::Unsupported);
        }
        let display = CString::new(display).map_err(|_| PlatformError::Unsupported)?;
        let mut atoms = Atoms::default();
        let mut window = 0;
        // SAFETY: NUL-terminated display and writable, ABI-matching output
        // structs live across the call. The returned allocation has one owner.
        let handle = NonNull::new(unsafe {
            ffi::fr_clip_open(display.as_ptr(), &raw mut atoms, &raw mut window)
        })
        .ok_or(PlatformError::Unavailable)?;
        Ok(Self {
            handle: Some(handle),
            atoms,
            window,
            limit: limits.max_clipboard_item_bytes() as usize,
            prepared: None,
            current: None,
            readers: std::array::from_fn(|_| None),
            capture: None,
        })
    }
    fn handle(&self) -> Result<*mut core::ffi::c_void, PlatformError> {
        self.handle
            .map(NonNull::as_ptr)
            .ok_or(PlatformError::Unavailable)
    }
    pub fn active_readers(&self) -> usize {
        self.readers.iter().flatten().count()
    }
    /// Query current ownership before using a stamp as echo provenance. This
    /// says nothing about application paste or clipboard-manager history.
    pub fn current_origin(&mut self) -> Result<Option<Stamp>, PlatformError> {
        let mut owner = 0;
        // SAFETY: live single-threaded handle and writable u32 output.
        if unsafe { ffi::fr_clip_owner(self.handle()?, &raw mut owner) } != 0 {
            return Err(PlatformError::Unavailable);
        }
        if owner != self.window {
            self.current = None;
        }
        Ok(self.current.as_ref().map(|s| s.stamp))
    }
    fn next(&mut self) -> Result<Option<Event>, PlatformError> {
        let mut event = Event::default();
        // SAFETY: a live uniquely accessed native owner and matching output.
        match unsafe { ffi::fr_clip_next(self.handle()?, &raw mut event) } {
            0 => Ok(None),
            1 => Ok(Some(event)),
            _ => Err(PlatformError::Unavailable),
        }
    }
    fn watch(&self, window: u32, enabled: bool) -> bool {
        let Ok(handle) = self.handle() else {
            return false;
        };
        // SAFETY: only a window integer is passed; XCB consumes BadWindow as an
        // ordinary error, never an Xlib process-global fatal handler.
        unsafe { ffi::fr_clip_watch(handle, window, i32::from(enabled)) == 0 }
    }
    fn property(&self, window: u32, property: u32, kind: u32, bytes: &[u8]) -> bool {
        let Ok(handle) = self.handle() else {
            return false;
        };
        if bytes.len() > CHUNK {
            return false;
        }
        // SAFETY: C copies precisely the validated slice before returning.
        unsafe {
            ffi::fr_clip_property(
                handle,
                window,
                property,
                kind,
                8,
                u32::try_from(bytes.len()).expect("bounded native chunk"),
                bytes.as_ptr().cast(),
            ) == 0
        }
    }
    fn words(&self, window: u32, property: u32, kind: u32, words: &[u32]) -> bool {
        let Ok(handle) = self.handle() else {
            return false;
        };
        if words.len() > CHUNK / 4 {
            return false;
        }
        // SAFETY: aligned u32 elements; the native byte order is what XCB's
        // format-32 interface expects. C copies before returning.
        unsafe {
            ffi::fr_clip_property(
                handle,
                window,
                property,
                kind,
                32,
                u32::try_from(words.len()).expect("bounded native words"),
                words.as_ptr().cast(),
            ) == 0
        }
    }
    fn notify(&self, event: &Event, property: u32) -> bool {
        let Ok(handle) = self.handle() else {
            return false;
        };
        // SAFETY: C reads the ABI-matching event and copies its own reply.
        unsafe { ffi::fr_clip_notify(handle, event, property) == 0 }
    }
    fn retire(&mut self, slot: usize) {
        if let Some(reader) = self.readers[slot].take() {
            self.watch(reader.window, false);
        }
    }
    fn request(&mut self, event: Event, now: Instant) {
        let Some(selection) = self.current.clone() else {
            self.notify(&event, 0);
            return;
        };
        let property = if event.property == 0 {
            event.target
        } else {
            event.property
        };
        let stale = event.time != 0 && event.time.wrapping_sub(selection.time) >= (1 << 31);
        if event.selection != self.atoms.clipboard || event.window == self.window || stale {
            self.notify(&event, 0);
            return;
        }
        // One transfer per requestor avoids property replacement and event-mask
        // ambiguity while that requestor has not consumed its previous value.
        if self
            .readers
            .iter()
            .flatten()
            .any(|r| r.window == event.window)
        {
            self.notify(&event, 0);
            return;
        }
        let accepted = if event.target == self.atoms.targets {
            self.words(
                event.window,
                property,
                4,
                &[self.atoms.targets, self.atoms.timestamp, self.atoms.utf8],
            )
        } else if event.target == self.atoms.timestamp {
            self.words(event.window, property, 19, &[selection.time])
        } else if event.target != self.atoms.utf8 {
            false
        } else if selection.text.0.len() <= CHUNK {
            self.property(event.window, property, self.atoms.utf8, &selection.text.0)
        } else if let Some(slot) = self.readers.iter().position(Option::is_none) {
            let size = u32::try_from(selection.text.0.len()).expect("admitted clipboard limit");
            if self.watch(event.window, true)
                && self.words(event.window, property, self.atoms.incr, &[size])
            {
                self.readers[slot] = Some(Reader {
                    window: event.window,
                    property,
                    selection,
                    offset: 0,
                    deadline: now + READER_LIFETIME,
                });
                true
            } else {
                self.watch(event.window, false);
                false
            }
        } else {
            false
        };
        if !self.notify(&event, if accepted { property } else { 0 }) {
            for slot in 0..READERS {
                if self.readers[slot]
                    .as_ref()
                    .is_some_and(|r| r.window == event.window)
                {
                    self.retire(slot);
                }
            }
        }
    }
    fn event(&mut self, event: Event, now: Instant) {
        if let Some(capture) = &mut self.capture {
            capture.observe(event, self.atoms);
        }
        match event.kind {
            1 => self.request(event, now),
            2 => {
                if self
                    .current
                    .as_ref()
                    .is_some_and(|s| event.time.wrapping_sub(s.time) < (1 << 31))
                {
                    self.current = None;
                }
            }
            3 => {
                if let Some(slot) = self.readers.iter().position(|r| {
                    r.as_ref()
                        .is_some_and(|r| r.window == event.window && r.property == event.property)
                }) {
                    let r = self.readers[slot].as_ref().expect("matched slot");
                    let end = (r.offset + CHUNK).min(r.selection.text.0.len());
                    let complete = r.offset == end;
                    let sent = now < r.deadline
                        && self.property(
                            r.window,
                            r.property,
                            self.atoms.utf8,
                            &r.selection.text.0[r.offset..end],
                        );
                    if !sent || complete {
                        self.retire(slot);
                    } else {
                        self.readers[slot].as_mut().expect("live reader").offset = end;
                    }
                }
            }
            5 => {
                for slot in 0..READERS {
                    if self.readers[slot]
                        .as_ref()
                        .is_some_and(|r| r.window == event.window)
                    {
                        self.retire(slot);
                    }
                }
            }
            _ => {}
        }
    }
    /// At most 32 events per call; neither event traffic nor acknowledgments
    /// renew a reader's three-second deadline. Aborted INCR never sends a false
    /// zero-length success terminator for partially delivered text.
    pub fn pump(&mut self) -> Result<usize, PlatformError> {
        let now = Instant::now();
        for slot in 0..READERS {
            if self.readers[slot]
                .as_ref()
                .is_some_and(|r| now >= r.deadline)
            {
                self.retire(slot);
            }
        }
        let mut count = 0;
        while count < 32 {
            let Some(event) = self.next()? else {
                break;
            };
            self.event(event, Instant::now());
            count += 1;
        }
        Ok(count)
    }
    /// Clear only private state and destroy this connection's windows. No
    /// selection reset is sent, so another app's newer selection is untouched.
    pub fn close(&mut self) {
        if let Some(handle) = self.handle.take() {
            // SAFETY: consumed unique allocation; this owner cannot use it again.
            unsafe {
                ffi::fr_clip_close(handle.as_ptr());
            }
        }
        self.capture = None;
        self.prepared = None;
        self.current = None;
        self.readers = std::array::from_fn(|_| None);
    }
}
impl ClipboardSink for X11Clipboard {
    fn prepare(&mut self, text: &str, stamp: Stamp) -> Result<(), PlatformError> {
        self.prepared = None;
        if text.len() > self.limit || !stamp.valid() {
            return Err(PlatformError::Unsupported);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(text.len())
            .map_err(|_| PlatformError::Unavailable)?;
        bytes.extend_from_slice(text.as_bytes());
        let text = Text(bytes);
        // Obtain a real server timestamp without changing selection ownership.
        // The core resamples authority after this potentially blocking phase.
        self.pump()?;
        let mut sequence = 0;
        // SAFETY: live uniquely owned connection and writable u32 output.
        if unsafe { ffi::fr_clip_tick(self.handle()?, &raw mut sequence) } != 0 {
            return Err(PlatformError::Unavailable);
        }
        let until = Instant::now() + PREPARE_LIFETIME;
        while Instant::now() < until {
            match self.next()? {
                Some(event) if event.kind == 4 && event.time != 0 && event.sequence == sequence => {
                    self.prepared = Some(Rc::new(Selection {
                        text,
                        stamp,
                        time: event.time,
                    }));
                    return Ok(());
                }
                Some(event) => self.event(event, Instant::now()),
                None => std::thread::sleep(Duration::from_millis(1)),
            }
        }
        Err(PlatformError::Unavailable)
    }
    fn publish(&mut self, _text: &str, stamp: Stamp) -> Publication {
        self.cancel_read();
        let Some(prepared) = self.prepared.take() else {
            return Publication::NotSubmitted(PlatformError::Unavailable);
        };
        if prepared.stamp != stamp {
            return Publication::NotSubmitted(PlatformError::Unavailable);
        }
        let Ok(handle) = self.handle() else {
            return Publication::NotSubmitted(PlatformError::Unavailable);
        };
        // SAFETY: final external operation uses the previously prepared server
        // timestamp, never CurrentTime. All text allocation happened beforehand.
        if unsafe { ffi::fr_clip_publish(handle, prepared.time) } != 0 {
            self.current = Some(prepared);
            return Publication::UnknownEffect;
        }
        let mut owner = 0;
        // SAFETY: synchronous confirmation after, not before, the OS submission.
        if unsafe { ffi::fr_clip_owner(handle, &raw mut owner) } != 0 {
            self.current = Some(prepared);
            return Publication::UnknownEffect;
        }
        if owner != self.window {
            self.current = None;
            return Publication::UnknownEffect;
        }
        self.current = Some(prepared);
        Publication::SubmittedToOs
    }
    fn cancel_prepared(&mut self) {
        self.prepared = None;
    }
}
impl Drop for X11Clipboard {
    fn drop(&mut self) {
        self.close();
    }
}
