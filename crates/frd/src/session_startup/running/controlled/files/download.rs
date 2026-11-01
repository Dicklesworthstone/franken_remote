//! The host's explicitly selected source, retained by the original controller.
//! Disk reads and source cleanup remain in fr-files' bounded sender worker.
use super::{ControlledHost, Error};
use asupersync::cx::Cx;
use fr_core::time::HostInstant;
use fr_files::{
    sender::{self, Policy, Sender, batch},
    session::{Authority, Permission},
};
use fr_transport::quic::{ChannelRequest, QuicRecords};
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
    /// Send one finite local selection on this controller's one-use download lane.
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
                batch: Some((selection, lifetime)),
                permission,
                policy,
            },
        )
    }
    /// Establish this controller's original download lane without selecting a
    /// source. Later local batches use the same sender, transfer IDs and peer
    /// quota owner; no empty transfer or publication is manufactured.
    pub(crate) fn offer_file_download_idle(
        &mut self,
        request: ChannelRequest,
        permission: Permission,
        policy: Policy,
    ) -> Result<(), Error> {
        crate::session_startup::native_control::file_download_lane(&self.session.opened.selected)
            .map_err(|_| Error::NotNegotiated)?;
        let handle = u128::from(request.binding.parent.id);
        self.offer_file_target(
            request,
            handle,
            super::negotiation::Target::Send {
                batch: None,
                permission,
                policy,
            },
        )
    }
    pub(crate) fn send_download_files(
        &mut self,
        selection: batch::Selection,
        lifetime: Duration,
    ) -> Result<(), sender::Error> {
        self.admit_file_download()?;
        self.files
            .download
            .sender
            .as_mut()
            .ok_or(sender::Error::Closed)?
            .sender
            .begin_batch(&self.session.opened.transport, selection, lifetime)
    }
    pub(crate) fn send_download_files_before(
        &mut self,
        selection: batch::Selection,
        until_us: u64,
    ) -> Result<(), sender::Error> {
        self.admit_file_download()?;
        self.files
            .download
            .sender
            .as_mut()
            .ok_or(sender::Error::Closed)?
            .sender
            .begin_batch_before(&self.session.opened.transport, selection, until_us)
    }
    pub(crate) fn can_select_download_files(&mut self) -> bool {
        self.file_send_stage() == Some(sender::Stage::Idle) && self.admit_file_download().is_ok()
    }
    fn admit_file_download(&mut self) -> Result<(), sender::Error> {
        self.session.check().map_err(|_| sender::Error::Closed)?;
        if self.files.download.pending.is_some() {
            return Err(sender::Error::Busy);
        }
        if self.control().is_stopped()
            || !self.renewal.permitted()
            || self.files.download.stopped
            || self.files.download.reason.is_some()
        {
            return Err(sender::Error::Closed);
        }
        let outgoing = self
            .files
            .download
            .sender
            .as_ref()
            .ok_or(sender::Error::Closed)?;
        let now = crate::session_startup::now(&outgoing.cx).map_err(|_| sender::Error::Clock)?;
        if !outgoing.permission.is_approved()
            || self.session.opened.control.check().is_err()
            || outgoing
                .authority
                .deadline(HostInstant::from_micros(now))
                .is_err()
        {
            return Err(sender::Error::Cancelled);
        }
        Ok(())
    }
    pub(crate) fn set_download_paused(&mut self, paused: bool) -> Result<(), sender::Error> {
        if self.files.download.stopped {
            return Err(sender::Error::Closed);
        }
        self.files
            .download
            .sender
            .as_mut()
            .ok_or(sender::Error::Closed)?
            .sender
            .set_paused(&self.session.opened.transport, paused)
    }
    pub(crate) fn download_is_paused(&self) -> Option<bool> {
        self.files
            .download
            .sender
            .as_ref()
            .map(|s| s.sender.is_paused())
    }
    pub(crate) fn file_send_failure(&self) -> Option<sender::Error> {
        self.files.download.reason.map(send_error)
    }
    /// Nonblocking original-source collection also works after parent closure.
    pub(crate) fn try_finish_download_cleanup(&mut self) -> Option<Result<(), sender::Error>> {
        if !self.files.download.stopped || self.files.download.pending.is_some() {
            return None;
        }
        match self.files.download.sender.as_mut() {
            Some(outgoing) => outgoing.sender.try_finish_cleanup(),
            None => Some(Ok(())),
        }
    }
    /// Original, ordered receipts. `ControllerPublished` means the receiver
    /// verified ATP and reported publication; transport acknowledgements do not.
    pub fn file_batch_report(&self) -> Option<batch::Report> {
        self.files
            .download
            .sender
            .as_ref()
            .and_then(|outgoing| outgoing.sender.batch_report())
    }
    pub fn take_file_batch_report(&mut self) -> Option<batch::Report> {
        self.files
            .download
            .sender
            .as_mut()
            .and_then(|outgoing| outgoing.sender.take_batch_report())
    }
    pub fn file_send_result(&self) -> Option<sender::Receipt> {
        self.files
            .download
            .sender
            .as_ref()
            .and_then(|outgoing| outgoing.sender.result())
    }
    pub fn file_send_progress(&self) -> Option<sender::Progress> {
        self.files
            .download
            .sender
            .as_ref()
            .and_then(|outgoing| outgoing.sender.progress())
    }
    pub fn file_send_stage(&self) -> Option<sender::Stage> {
        self.files
            .download
            .sender
            .as_ref()
            .map(|outgoing| outgoing.sender.stage())
    }
    pub fn file_send_negotiating(&self) -> bool {
        self.files
            .download
            .pending
            .as_ref()
            .is_some_and(super::negotiation::Pending::is_download)
    }
}

pub(in crate::session_startup::running) fn send_error(error: Error) -> sender::Error {
    match error {
        Error::Sender(error) => error,
        Error::Transport(error) => sender::Error::Transport(error),
        Error::AlreadyAttached => sender::Error::Busy,
        Error::NotNegotiated | Error::WrongBinding => sender::Error::WrongRole,
        Error::PermissionRequired | Error::Cancelled => sender::Error::Cancelled,
        Error::Clock => sender::Error::Clock,
        Error::Closed | Error::CleanupPending | Error::Receiver(_) => sender::Error::Closed,
    }
}
