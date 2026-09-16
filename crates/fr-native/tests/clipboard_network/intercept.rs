//! Deterministic blocking around a REAL X11 preparation, never fake publication.
use fr_core::clipboard::{ClipboardSink, PlatformError, Publication, Stamp};
use fr_native::clipboard::{ReadText, SynchronizationError, X11Clipboard};
use fr_wire::clipboard::session::synchronize::{NativeChanges, NativeClipboard};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
#[derive(Default)]
pub struct Pause {
    pub enabled: AtomicBool,
    pub entered: AtomicBool,
    pub release: AtomicBool,
}
pub struct Native {
    pub clipboard: X11Clipboard,
    pub pause: Arc<Pause>,
}
impl ClipboardSink for Native {
    fn prepare(&mut self, text: &str, stamp: Stamp) -> Result<(), PlatformError> {
        self.clipboard.prepare(text, stamp)
    }
    fn publish(&mut self, text: &str, stamp: Stamp) -> Publication {
        self.clipboard.publish(text, stamp)
    }
    fn cancel_prepared(&mut self) {
        self.clipboard.cancel_prepared();
    }
}
impl NativeClipboard for Native {
    type Text = ReadText;
    type Error = SynchronizationError;
    fn watch(&mut self) -> Result<u64, Self::Error> {
        NativeClipboard::watch(&mut self.clipboard)
    }
    fn changes(&mut self) -> Result<NativeChanges, Self::Error> {
        NativeClipboard::changes(&mut self.clipboard)
    }
    fn revision(&self) -> u64 {
        self.clipboard.change_revision()
    }
    fn prepare_for_revision(
        &mut self,
        text: &str,
        stamp: Stamp,
        revision: u64,
    ) -> Result<(), PlatformError> {
        self.clipboard.prepare_for_revision(text, stamp, revision)?;
        if self.pause.enabled.load(Ordering::Acquire) {
            self.pause.entered.store(true, Ordering::Release);
            let until = Instant::now() + Duration::from_secs(2);
            while !self.pause.release.load(Ordering::Acquire) {
                if Instant::now() >= until {
                    return Err(PlatformError::Unavailable);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }
    fn begin_read(&mut self) -> Result<(), Self::Error> {
        NativeClipboard::begin_read(&mut self.clipboard)
    }
    fn poll_read(&mut self) -> Result<Option<ReadText>, Self::Error> {
        NativeClipboard::poll_read(&mut self.clipboard)
    }
    fn cancel_read(&mut self) {
        self.clipboard.cancel_read();
    }
    fn suspend(&mut self) -> Result<(), Self::Error> {
        NativeClipboard::suspend(&mut self.clipboard)
    }
    fn close(&mut self) {
        self.clipboard.close();
    }
}
