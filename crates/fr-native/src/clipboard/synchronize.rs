//! Actual X11 backend for the safe, runtime-independent synchronization owner.
use super::{
    ClipboardSink, PlatformError, ReadError, ReadText, Stamp, WatchError, X11Clipboard, ffi,
};
use fr_wire::clipboard::session::synchronize::{
    NativeChange, NativeChanges, NativeClipboard, NativeText,
};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynchronizationError {
    Watch(WatchError),
    Read(ReadError),
}
/// An admitted channel plus its native X11 clipboard; no synthetic viewer lease.
pub type ClipboardSynchronizer =
    fr_wire::clipboard::session::synchronize::Synchronizer<X11Clipboard>;

impl NativeText for ReadText {
    fn text(&self) -> &str {
        self.as_str()
    }
    fn origin(&self) -> Option<Stamp> {
        self.origin()
    }
}
impl NativeClipboard for X11Clipboard {
    type Text = ReadText;
    type Error = SynchronizationError;
    fn watch(&mut self) -> Result<u64, Self::Error> {
        self.start_watching().map_err(SynchronizationError::Watch)?;
        Ok(self.change_revision())
    }
    fn changes(&mut self) -> Result<NativeChanges, Self::Error> {
        let (latest, settled) = self
            .poll_change_turn()
            .map_err(SynchronizationError::Watch)?;
        Ok(NativeChanges {
            latest: latest.map(|change| NativeChange {
                revision: change.revision(),
                has_selection: change.has_selection(),
                origin: change.origin(),
            }),
            settled,
        })
    }
    fn revision(&self) -> u64 {
        self.change_revision()
    }
    fn prepare_for_revision(
        &mut self,
        text: &str,
        stamp: Stamp,
        revision: u64,
    ) -> Result<(), PlatformError> {
        if self.change_revision() != revision {
            return Err(PlatformError::LocalChanged);
        }
        self.prepare(text, stamp)?;
        let prepared = self
            .prepared
            .as_ref()
            .ok_or(PlatformError::Unavailable)?
            .time;
        let until = Instant::now() + super::PREPARE_LIFETIME;
        let mut events = 0;
        while Instant::now() < until && events < 32 {
            let mut sequence = 0;
            // SAFETY: live thread-confined XCB handle and writable scalar. The
            // server timestamp barrier makes *later* changes strictly newer
            // than our prepared timestamp, even within a millisecond tick.
            if unsafe { ffi::fr_clip_tick(self.handle()?, &raw mut sequence) } != 0 {
                return Err(PlatformError::Unavailable);
            }
            while Instant::now() < until && events < 32 {
                let Some(event) = self.next()? else {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                };
                events += 1;
                self.event(event, Instant::now());
                // Retain metadata for the next synchronization turn: discovering
                // a new copy must not swallow its later automatic propagation.
                if self.change_revision() != revision {
                    return Err(PlatformError::LocalChanged);
                }
                if event.kind == 4 && event.sequence == sequence {
                    let elapsed = event.time.wrapping_sub(prepared);
                    if elapsed > 0 && elapsed < (1 << 31) {
                        return Ok(());
                    }
                    if elapsed >= (1 << 31) {
                        return Err(PlatformError::Unavailable);
                    }
                    // Equality is ambiguous at X11's millisecond resolution.
                    // Wait for a real server tick; never fabricate a timestamp.
                    std::thread::sleep(Duration::from_millis(1));
                    break;
                }
            }
        }
        Err(PlatformError::Unavailable)
    }
    fn begin_read(&mut self) -> Result<(), Self::Error> {
        self.begin_read().map_err(SynchronizationError::Read)
    }
    fn poll_read(&mut self) -> Result<Option<Self::Text>, Self::Error> {
        self.poll_read().map_err(SynchronizationError::Read)
    }
    fn cancel_read(&mut self) {
        self.cancel_read();
    }
    fn suspend(&mut self) -> Result<(), Self::Error> {
        self.cancel_read();
        self.prepared = None;
        self.current = None;
        for slot in 0..super::READERS {
            self.retire(slot);
        }
        self.stop_watching().map_err(SynchronizationError::Watch)
    }
    fn close(&mut self) {
        self.close();
    }
}
