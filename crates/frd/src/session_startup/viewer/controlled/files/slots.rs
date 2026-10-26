//! Two fixed file directions on the original controller. Each retains its
//! own permission, one-use attachment and disk owner through terminal cleanup.
use super::super::ViewerControl;
use super::{Cx, Error, QuicRecords, Route, Slot};
use fr_client::input::presentation::PresentedInput;

#[derive(Default)]
pub(in crate::session_startup::viewer::controlled) struct Slots {
    pub(super) upload: Slot,
    pub(super) download: Slot,
    next_download: bool,
}
impl Slots {
    pub(super) fn slot(&self, download: bool) -> &Slot {
        if download {
            &self.download
        } else {
            &self.upload
        }
    }
    pub(super) fn slot_mut(&mut self, download: bool) -> &mut Slot {
        if download {
            &mut self.download
        } else {
            &mut self.upload
        }
    }
    pub(in crate::session_startup::viewer::controlled) fn owns(
        &self,
        route: Route,
        bytes: &[u8],
    ) -> bool {
        self.upload.owns(route, bytes) || self.download.owns(route, bytes)
    }
    pub(in crate::session_startup::viewer::controlled) fn deadline_us(&self) -> Option<u64> {
        self.upload
            .deadline_us()
            .into_iter()
            .chain(self.download.deadline_us())
            .min()
    }
    pub(in crate::session_startup::viewer::controlled) fn check_pending(
        &mut self,
    ) -> Result<(), Error> {
        let upload = self.upload.check_pending();
        let download = self.download.check_pending();
        upload.and(download)
    }
    pub(in crate::session_startup::viewer::controlled) fn permits_io(&mut self) -> bool {
        self.upload.permits_io() & self.download.permits_io()
    }
    pub(in crate::session_startup::viewer::controlled) fn stop(
        &mut self,
        q: &mut QuicRecords,
    ) -> Result<(), Error> {
        self.stop_on(q, None)
    }
    pub(in crate::session_startup::viewer::controlled) fn stop_on(
        &mut self,
        q: &mut QuicRecords,
        cleanup: Option<&Cx>,
    ) -> Result<(), Error> {
        let upload = self.upload.stop_on(q, cleanup);
        let download = self.download.stop_on(q, cleanup);
        upload.and(download)
    }
    pub(in crate::session_startup::viewer::controlled) fn prepare_pending_receive(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        self.upload.prepare_pending_receive(q, &mut authorize)?;
        self.download.prepare_pending_receive(q, authorize)
    }
    /// The caller services input and renewal before this bounded pair. Alternate
    /// the first direction so shared bulk backpressure cannot favor one forever.
    pub(in crate::session_startup::viewer::controlled) fn service(
        &mut self,
        q: &mut QuicRecords,
        input: &mut PresentedInput,
        control: &ViewerControl,
        cx: &Cx,
    ) -> Result<(), Error> {
        let first = self.next_download;
        self.next_download = !first;
        self.slot_mut(first).service(q, input, control, cx)?;
        self.slot_mut(!first).service(q, input, control, cx)
    }
}
