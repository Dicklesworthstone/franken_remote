//! Native synchronization using the original controller's projected clock.
use super::{ControllerClipboard, Error, ProjectedClock};
use crate::input::ClientInstant;
use fr_core::clipboard::ClipboardSwitch;
use fr_wire::clipboard::session::{
    RecordSink,
    synchronize::{
        IdentifierFailure, NativeClipboard, Progress, Received, SyncError, Synchronizer,
    },
};
use std::{cell::Cell, fmt};

#[derive(Debug)]
pub enum NativeError<E> {
    Authority(Error),
    Synchronizer(SyncError<E>),
}
/// Runs on the platform's interactive worker. This creates neither a thread nor
/// an async runtime. Poll during silence; every callback samples CLIENT time.
pub struct ControllerSynchronizer<N: NativeClipboard> {
    native: Synchronizer<N>,
    clock: ProjectedClock,
}
impl<N: NativeClipboard> fmt::Debug for ControllerSynchronizer<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControllerSynchronizer")
            .field("closed", &self.native.is_closed())
            .finish_non_exhaustive()
    }
}
impl ControllerClipboard {
    pub fn into_native<N: NativeClipboard>(self, native: N) -> ControllerSynchronizer<N> {
        ControllerSynchronizer {
            native: Synchronizer::new(self.channel, native),
            clock: self.clock,
        }
    }
}
impl<N: NativeClipboard> ControllerSynchronizer<N> {
    pub fn local_switch(&self) -> ClipboardSwitch {
        self.native.local_switch()
    }
    pub fn peer_switch(&self) -> ClipboardSwitch {
        self.native.peer_switch()
    }
    pub fn close(&mut self) {
        self.native.close();
    }
    pub fn is_closed(&self) -> bool {
        self.native.is_closed()
    }
    pub fn retained_channel_bytes(&self) -> usize {
        self.native.retained_channel_bytes()
    }
    pub fn poll(
        &mut self,
        scratch: &mut [u8],
        sink: &mut impl RecordSink,
        mut clock: impl FnMut() -> ClientInstant,
        new_id: impl FnMut() -> Result<u128, IdentifierFailure>,
    ) -> Result<Progress<N::Error>, NativeError<N::Error>> {
        let failure = Cell::new(None);
        let result = self.native.poll(
            scratch,
            sink,
            || self.clock.checked(clock(), &failure),
            new_id,
        );
        result.map_err(|error| {
            failure
                .get()
                .map_or(NativeError::Synchronizer(error), NativeError::Authority)
        })
    }
    pub fn receive(
        &mut self,
        record: &[u8],
        mut clock: impl FnMut() -> ClientInstant,
    ) -> Result<Received, NativeError<N::Error>> {
        let failure = Cell::new(None);
        let result = self
            .native
            .receive(record, || self.clock.checked(clock(), &failure));
        result.map_err(|error| {
            failure
                .get()
                .map_or(NativeError::Synchronizer(error), NativeError::Authority)
        })
    }
}
