//! The host's explicitly selected source, retained by the original controller.
//! Disk reads and source cleanup remain in fr-files' bounded sender worker.
use super::{ControlledHost, Error};
use asupersync::cx::Cx;
use fr_core::time::HostInstant;
use fr_files::{
    sender::{self, Policy, Sender, batch},
    session::{Authority, Permission},
};
use fr_transport::quic::{self, ChannelRequest, QuicRecords};
use std::time::Duration;

pub(super) struct Outgoing {
    pub(super) sender: Sender<'static>,
    pub(super) authority: Authority,
    pub(super) permission: Permission,
    pub(super) cx: Cx,
}
impl Outgoing {
    pub(super) fn service(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), sender::Error> {
        let authority = &self.authority;
        let permission = &self.permission;
        let cx = &self.cx;
        self.sender
            .service(q, || {
                permission.is_approved()
                    && authorize()
                    && crate::session_startup::now(cx)
                        .is_ok_and(|at| authority.deadline(HostInstant::from_micros(at)).is_ok())
            })
            .map(|_| ())
    }
}

impl ControlledHost {
    /// Send one finite local selection on this controller's one-use Files lane.
    /// Only positive download negotiation permits this direction. The selection
    /// contains pinned local descriptors; no peer path or browse request exists.
    /// Ordinary turns finish attachment before starting any source worker.
    pub fn offer_file_download(
        &mut self,
        request: ChannelRequest,
        selection: batch::Selection,
        permission: Permission,
        policy: Policy,
        lifetime: Duration,
    ) -> Result<(), Error> {
        crate::session_startup::native_control::file_download_lane(&self.session.opened.selected)
            .map_err(|_| Error::NotNegotiated)?;
        let handle = u128::from(request.binding.parent.id);
        self.offer_file_target(
            request,
            handle,
            super::negotiation::Target::Send {
                selection,
                permission,
                policy,
                lifetime,
            },
        )
    }
    /// Original, ordered receipts. ControllerPublished means the receiver
    /// verified ATP and reported publication; transport acknowledgements do not.
    pub fn file_batch_report(&self) -> Option<batch::Report> {
        self.files
            .sender
            .as_ref()
            .and_then(|outgoing| outgoing.sender.batch_report())
    }
    pub fn take_file_batch_report(&mut self) -> Option<batch::Report> {
        self.files
            .sender
            .as_mut()
            .and_then(|outgoing| outgoing.sender.take_batch_report())
    }
    pub fn file_send_result(&self) -> Option<sender::Receipt> {
        self.files
            .sender
            .as_ref()
            .and_then(|outgoing| outgoing.sender.result())
    }
    pub fn file_send_progress(&self) -> Option<sender::Progress> {
        self.files
            .sender
            .as_ref()
            .and_then(|outgoing| outgoing.sender.progress())
    }
    pub fn file_send_stage(&self) -> Option<sender::Stage> {
        self.files
            .sender
            .as_ref()
            .map(|outgoing| outgoing.sender.stage())
    }
    pub fn file_send_negotiating(&self) -> bool {
        self.files
            .pending
            .as_ref()
            .is_some_and(super::negotiation::Pending::is_download)
    }
}
