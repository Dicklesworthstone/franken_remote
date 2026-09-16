//! Explicit X11 selection observation. No authority, automatic paste or network
//! send is implied. All server operations belong on the native worker, outside
//! the broker's authority lock; the X server remains an OS trust boundary.
use super::{Atoms, CHUNK, Event, READER_LIFETIME, Stamp, Text, X11Clipboard, ffi};
use core::fmt;
use fr_core::clipboard::{MAX_CHUNKS, PlatformError};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    Platform(PlatformError),
    Busy,
    NotReading,
    NoSelection,
    Expired,
    LocalChanged,
    Limit,
    Allocation,
    Unsupported,
    InvalidUtf8,
    Malformed,
}
impl From<PlatformError> for ReadError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}
impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ReadError {}

/// One complete validated selection. Neither cloneable nor content-debuggable;
/// its private bytes are cleared on drop. An origin exists only for a selection
/// still owned by this adapter, never by comparing text with a previous copy.
pub struct ReadText {
    text: Text,
    origin: Option<Stamp>,
}
impl ReadText {
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.text.0).expect("validated complete selection")
    }
    pub const fn origin(&self) -> Option<Stamp> {
        self.origin
    }
}
impl fmt::Debug for ReadText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClipboardReadText([redacted])")
    }
}

#[derive(Clone, Copy)]
enum Stage {
    Clock(u32),
    Before,
    Selection,
    Data {
        incremental: bool,
        offset: u32,
        total: Option<usize>,
    },
    NextChunk(u32),
    After,
}
pub(super) struct Capture {
    window: u32,
    owner: u32,
    deadline: Instant,
    stage: Stage,
    signal: Option<Event>,
    time: u32,
    timestamp: u32,
    lower_bound: usize,
    chunks: u32,
    text: Text,
    origin: Option<Stamp>,
}
impl Capture {
    pub(super) fn observe(&mut self, event: Event, atoms: Atoms) {
        let expected = match self.stage {
            Stage::Clock(sequence) => event.kind == 4 && event.sequence == sequence,
            Stage::Before | Stage::Selection | Stage::After => {
                let (target, property) = self.target(atoms);
                event.kind == 6
                    && event.window == self.window
                    && event.selection == atoms.clipboard
                    && event.target == target
                    && (event.property == property || event.property == 0)
                    && event.time == self.time
            }
            Stage::NextChunk(sequence) => {
                event.kind == 7
                    && event.window == self.window
                    && event.property == atoms.utf8
                    && event.sequence.wrapping_sub(sequence) < (1 << 31)
            }
            Stage::Data { .. } => false,
        };
        if expected && self.signal.is_none() {
            self.signal = Some(event);
        }
    }
    fn target(&self, atoms: Atoms) -> (u32, u32) {
        match self.stage {
            Stage::Before => (atoms.timestamp, atoms.timestamp),
            Stage::After => (atoms.timestamp, atoms.clock),
            _ => (atoms.utf8, atoms.utf8),
        }
    }
    fn current(&self, clipboard: &X11Clipboard) -> Result<(), ReadError> {
        if Instant::now() >= self.deadline {
            return Err(ReadError::Expired);
        }
        let mut owner = 0;
        // SAFETY: live connection and synchronous, writable scalar output.
        if unsafe { ffi::fr_clip_owner(clipboard.handle()?, &raw mut owner) } != 0 {
            return Err(PlatformError::Unavailable.into());
        }
        if owner != self.owner {
            return Err(ReadError::LocalChanged);
        }
        if Instant::now() >= self.deadline {
            return Err(ReadError::Expired);
        }
        Ok(())
    }
    fn convert(&mut self, clipboard: &X11Clipboard, stage: Stage) -> Result<(), ReadError> {
        self.stage = stage;
        self.signal = None;
        let (target, property) = self.target(clipboard.atoms);
        // SAFETY: native scalar IDs belong to this live connection; C retains
        // no pointers. A distinct verification property fences delayed replies.
        if unsafe {
            ffi::fr_clip_convert(
                clipboard.handle()?,
                self.window,
                target,
                property,
                self.time,
            )
        } != 0
        {
            return Err(PlatformError::Unavailable.into());
        }
        Ok(())
    }
    fn property(
        &self,
        clipboard: &X11Clipboard,
        property: u32,
        offset: u32,
        bytes: &mut [u8; CHUNK],
    ) -> Result<ffi::Property, ReadError> {
        let mut reply = ffi::Property::default();
        // SAFETY: C copies at most CHUNK bytes into the live stack allocation,
        // with an ABI-matching metadata output. No caller buffer is retained.
        if unsafe {
            ffi::fr_clip_read(
                clipboard.handle()?,
                self.window,
                property,
                offset,
                &raw mut reply,
                bytes.as_mut_ptr(),
            )
        } != 0
        {
            return Err(PlatformError::Unavailable.into());
        }
        Ok(reply)
    }
    fn delete(&self, clipboard: &X11Clipboard) -> Result<u32, ReadError> {
        let mut sequence = 0;
        // SAFETY: live owned requestor, scalar atom and synchronous output.
        if unsafe {
            ffi::fr_clip_delete(
                clipboard.handle()?,
                self.window,
                clipboard.atoms.utf8,
                &raw mut sequence,
            )
        } != 0
        {
            return Err(PlatformError::Unavailable.into());
        }
        Ok(sequence)
    }
    // At most one 16 KiB GetProperty per call. The length in an INCR header is
    // a lower bound, not an allocation instruction or an exact final length.
    fn step(
        &mut self,
        clipboard: &X11Clipboard,
        scratch: &mut [u8; CHUNK],
    ) -> Result<bool, ReadError> {
        match self.stage {
            Stage::Clock(_) => {
                let Some(event) = self.signal.take() else {
                    return Ok(false);
                };
                if event.time == 0 {
                    return Err(ReadError::Malformed);
                }
                self.time = event.time;
                self.convert(clipboard, Stage::Before)?;
            }
            Stage::Before | Stage::After => {
                let Some(event) = self.signal.take() else {
                    return Ok(false);
                };
                if event.property == 0 {
                    return Err(if matches!(self.stage, Stage::After) {
                        ReadError::LocalChanged
                    } else {
                        ReadError::Unsupported
                    });
                }
                let (_, property) = self.target(clipboard.atoms);
                let reply = self.property(clipboard, property, 0, scratch)?;
                if reply.kind != 19 || reply.format != 32 || reply.len != 4 || reply.remaining != 0
                {
                    return Err(ReadError::Malformed);
                }
                let timestamp = u32::from_ne_bytes(scratch[..4].try_into().expect("four bytes"));
                if timestamp == 0 || self.time.wrapping_sub(timestamp) >= (1 << 31) {
                    return Err(ReadError::LocalChanged);
                }
                if matches!(self.stage, Stage::After) {
                    if timestamp != self.timestamp {
                        return Err(ReadError::LocalChanged);
                    }
                    if self.text.0.len() < self.lower_bound {
                        return Err(ReadError::Malformed);
                    }
                    std::str::from_utf8(&self.text.0).map_err(|_| ReadError::InvalidUtf8)?;
                    return Ok(true);
                }
                self.timestamp = timestamp;
                self.convert(clipboard, Stage::Selection)?;
            }
            Stage::Selection => {
                let Some(event) = self.signal.take() else {
                    return Ok(false);
                };
                if event.property == 0 {
                    return Err(ReadError::Unsupported);
                }
                let reply = self.property(clipboard, clipboard.atoms.utf8, 0, scratch)?;
                if reply.kind == clipboard.atoms.incr {
                    if reply.format != 32 || reply.len != 4 || reply.remaining != 0 {
                        return Err(ReadError::Malformed);
                    }
                    self.lower_bound =
                        u32::from_ne_bytes(scratch[..4].try_into().expect("four bytes")) as usize;
                    if self.lower_bound > clipboard.limit {
                        return Err(ReadError::Limit);
                    }
                    self.stage = Stage::NextChunk(self.delete(clipboard)?);
                } else {
                    self.data(clipboard, &reply, scratch, false, 0, None)?;
                }
            }
            Stage::NextChunk(_) => {
                if self.signal.take().is_none() {
                    return Ok(false);
                }
                self.chunks = self.chunks.checked_add(1).ok_or(ReadError::Limit)?;
                // One extra property is the mandatory empty terminator.
                if self.chunks > MAX_CHUNKS + 1 {
                    return Err(ReadError::Limit);
                }
                let reply = self.property(clipboard, clipboard.atoms.utf8, 0, scratch)?;
                self.data(clipboard, &reply, scratch, true, 0, None)?;
            }
            Stage::Data {
                incremental,
                offset,
                total,
            } => {
                let reply = self.property(clipboard, clipboard.atoms.utf8, offset, scratch)?;
                self.data(clipboard, &reply, scratch, incremental, offset, total)?;
            }
        }
        Ok(false)
    }
    fn data(
        &mut self,
        clipboard: &X11Clipboard,
        reply: &ffi::Property,
        scratch: &[u8; CHUNK],
        incremental: bool,
        offset: u32,
        total: Option<usize>,
    ) -> Result<(), ReadError> {
        if reply.kind != clipboard.atoms.utf8 || reply.format != 8 || reply.len as usize > CHUNK {
            return Err(ReadError::Malformed);
        }
        let len = reply.len as usize;
        let remaining = reply.remaining as usize;
        let size = len.checked_add(remaining).ok_or(ReadError::Limit)?;
        if self
            .text
            .0
            .len()
            .checked_add(size)
            .is_none_or(|size| size > clipboard.limit)
        {
            return Err(ReadError::Limit);
        }
        let property_size = (offset as usize)
            .checked_mul(4)
            .and_then(|n| n.checked_add(size))
            .ok_or(ReadError::Limit)?;
        if total.is_some_and(|total| total != property_size)
            || (remaining != 0 && (len == 0 || !len.is_multiple_of(4)))
        {
            return Err(ReadError::Malformed);
        }
        if incremental && self.chunks > MAX_CHUNKS && property_size != 0 {
            return Err(ReadError::Limit);
        }
        self.text
            .0
            .try_reserve_exact(len)
            .map_err(|_| ReadError::Allocation)?;
        self.text.0.extend_from_slice(&scratch[..len]);
        if remaining != 0 {
            self.stage = Stage::Data {
                incremental,
                offset: offset.checked_add(reply.len / 4).ok_or(ReadError::Limit)?,
                total: Some(property_size),
            };
        } else if incremental && property_size != 0 {
            self.stage = Stage::NextChunk(self.delete(clipboard)?);
        } else {
            self.delete(clipboard)?;
            self.convert(clipboard, Stage::After)?;
        }
        Ok(())
    }
}
struct Scratch([u8; CHUNK]);
impl Drop for Scratch {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

impl X11Clipboard {
    /// Start one bounded `UTF8_STRING` observation of the current CLIPBOARD.
    /// The caller must authorize observation BEFORE calling, service polling,
    /// cancel on either off switch/revoke, and recheck authority before sending.
    /// A new native requestor window fences all old responses without allocating
    /// a new server atom per operation. Timestamp-less owners refuse explicitly.
    pub fn begin_read(&mut self) -> Result<(), ReadError> {
        if self.capture.is_some() {
            return Err(ReadError::Busy);
        }
        let deadline = Instant::now() + READER_LIFETIME;
        let mut owner = 0;
        let handle = self.handle()?;
        // SAFETY: live unique connection with writable native scalar output.
        if unsafe { ffi::fr_clip_owner(handle, &raw mut owner) } != 0 {
            return Err(PlatformError::Unavailable.into());
        }
        if owner == 0 {
            return Err(ReadError::NoSelection);
        }
        let mut window = 0;
        // SAFETY: C creates one owned child and writes its scalar ID.
        if unsafe { ffi::fr_clip_requestor(handle, &raw mut window) } != 0 {
            return Err(PlatformError::Unavailable.into());
        }
        let mut sequence = 0;
        // SAFETY: live connection and writable scalar; timestamp comes from
        // its correlated real server notification, never CurrentTime.
        if unsafe { ffi::fr_clip_tick(handle, &raw mut sequence) } != 0 {
            // SAFETY: this fresh child is uniquely owned and no longer used.
            unsafe {
                ffi::fr_clip_destroy(handle, window);
            }
            return Err(PlatformError::Unavailable.into());
        }
        self.capture = Some(Capture {
            window,
            owner,
            deadline,
            stage: Stage::Clock(sequence),
            signal: None,
            time: 0,
            timestamp: 0,
            lower_bound: 0,
            chunks: 0,
            text: Text(Vec::new()),
            origin: if owner == self.window {
                self.current.as_ref().map(|s| s.stamp)
            } else {
                None
            },
        });
        Ok(())
    }
    /// Service at most 32 native events and one 16 KiB property read. None means
    /// pending, not empty text. The fixed deadline is checked even in silence
    /// and again after native calls. Errors retire the read and clear all bytes;
    /// partial or old-owner text is never exposed as a completed observation.
    pub fn poll_read(&mut self) -> Result<Option<ReadText>, ReadError> {
        if self.capture.is_none() {
            return Err(ReadError::NotReading);
        }
        if let Err(error) = self.pump() {
            self.cancel_read();
            return Err(error.into());
        }
        let mut capture = self.capture.take().expect("active read");
        let mut scratch = Scratch([0; CHUNK]);
        let result = capture
            .current(self)
            .and_then(|()| capture.step(self, &mut scratch.0))
            .and_then(|complete| capture.current(self).map(|()| complete));
        match result {
            Ok(false) => {
                self.capture = Some(capture);
                Ok(None)
            }
            result => {
                if let Ok(handle) = self.handle() {
                    // SAFETY: retires only this capture's uniquely owned child.
                    unsafe {
                        ffi::fr_clip_destroy(handle, capture.window);
                    }
                }
                result.map(|_| {
                    Some(ReadText {
                        text: capture.text,
                        origin: capture.origin,
                    })
                })
            }
        }
    }
    /// Cancel only this read, never reset or overwrite the OS clipboard. A
    /// later read gets a different window, so late INCR/replies cannot attach.
    pub fn cancel_read(&mut self) {
        if let Some(capture) = self.capture.take()
            && let Ok(handle) = self.handle()
        {
            // SAFETY: child is consumed once and no longer used by this owner.
            unsafe {
                ffi::fr_clip_destroy(handle, capture.window);
            }
        }
    }
}
