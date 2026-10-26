//! Exactly one upload and one download on the original controller. Each slot
//! keeps its own spent attachment, permission, disk owner and terminal result.
use super::{QuicRecords, Route, Slot, quic};

#[derive(Default)]
pub(in crate::session_startup::running::controlled) struct Slots {
    pub(in crate::session_startup::running::controlled) upload: Slot,
    pub(in crate::session_startup::running::controlled) download: Slot,
    next_download: bool,
}
impl Slots {
    pub(in crate::session_startup::running::controlled) fn slot(&self, download: bool) -> &Slot {
        if download {
            &self.download
        } else {
            &self.upload
        }
    }
    pub(in crate::session_startup::running::controlled) fn slot_mut(
        &mut self,
        download: bool,
    ) -> &mut Slot {
        if download {
            &mut self.download
        } else {
            &mut self.upload
        }
    }
    pub(in crate::session_startup::running::controlled) fn stop(&mut self) {
        self.upload.stop();
        self.download.stop();
    }
    pub(in crate::session_startup::running::controlled) fn owns(
        &self,
        route: Route,
        bytes: &[u8],
    ) -> bool {
        self.upload.owns(route, bytes) || self.download.owns(route, bytes)
    }
    pub(in crate::session_startup::running::controlled) fn permitted(&mut self) -> bool {
        self.upload.permitted() & self.download.permitted()
    }
    pub(in crate::session_startup::running::controlled) fn prepare_pending_receive(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), quic::Error> {
        self.upload.prepare_pending_receive(q, &mut authorize)?;
        self.download.prepare_pending_receive(q, authorize)
    }
    /// The containing turn has already serviced input and renewal. Visit each
    /// bounded owner once and alternate first service under shared bulk pressure.
    pub(in crate::session_startup::running::controlled) fn service(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), quic::Error> {
        let first = self.next_download;
        self.next_download = !first;
        self.slot_mut(first).service(q, &mut authorize)?;
        self.slot_mut(!first).service(q, authorize)
    }
}
