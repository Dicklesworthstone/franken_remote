//! Actual X11 backend for the safe, runtime-independent synchronization owner.
use super::{ReadError, ReadText, Stamp, WatchError, X11Clipboard};
use fr_wire::clipboard::session::synchronize::{
    NativeChange, NativeChanges, NativeClipboard, NativeText,
};

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
