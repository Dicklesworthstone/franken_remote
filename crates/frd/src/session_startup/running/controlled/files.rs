//! Optional ATP receipt owned by the original running controller.
//!
//! Disk work stays in fr-files' single bounded worker. The regular control turn
//! services this lane only after input and renewal; no application callback sees
//! its records. Retirement never erases an already committed publication.
mod download;
mod negotiation;
mod slots;
use super::ControlledHost;
use crate::worker::Deadline;
use asupersync::{cx::Cx, time::sleep_until, types::Time};
use fr_files::{
    quic::{Configuration, HostReceiver, State},
    session::Progress,
    wire::ResultReceipt,
    worker,
};
use fr_transport::quic::{self, MediaChannel, QuicRecords, Route, files::FilesChannel};
use fr_wire::{files, negotiation::Role};
use negotiation::{Advance, Pending};
pub(super) use slots::Slots;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    AlreadyAttached,
    NotNegotiated,
    PermissionRequired,
    WrongBinding,
    Closed,
    Clock,
    Cancelled,
    CleanupPending,
    Transport(quic::Error),
    Receiver(fr_files::quic::Error),
    Sender(fr_files::sender::Error),
}

/// Finished means the original disk thread was joined, not just asked to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleanup {
    NotStarted,
    Pending,
    Finished(Result<(), worker::Error>),
}

#[derive(Default)]
pub(super) struct Slot {
    used: bool,
    pending: Option<Pending>,
    receiver: Option<HostReceiver>,
    sender: Option<Box<download::Outgoing>>,
    reason: Option<Error>,
    cleanup: Option<Result<(), worker::Error>>,
}
impl Slot {
    pub(super) fn stop(&mut self) {
        if self.pending.take().is_some() {
            self.reason.get_or_insert(Error::Cancelled);
        }
        if let Some(receiver) = &mut self.receiver {
            receiver.stop();
        }
        if let Some(outgoing) = &mut self.sender {
            outgoing.sender.stop();
        }
    }
    pub(super) fn owns(&self, route: Route, bytes: &[u8]) -> bool {
        self.pending.as_ref().is_some_and(|p| p.owns(route, bytes))
            || self
                .receiver
                .as_ref()
                .is_some_and(|r| r.owns_inbound(route))
            || self
                .sender
                .as_ref()
                .is_some_and(|s| s.sender.owns_inbound(route))
    }
    /// Recheck the pending exchange during actual UDP I/O, not just admission.
    pub(super) fn permitted(&mut self) -> bool {
        match self.pending.as_mut().map(Pending::check) {
            Some(Err(error)) => {
                self.reason.get_or_insert(Error::Transport(error));
                false
            }
            _ => true,
        }
    }
    /// Retire or promote a proven final Files exchange before generic receive
    /// can frame its reset or first payload under the old handshake route. The
    /// pending owner remains here until post-input service starts its worker.
    pub(super) fn prepare_pending_receive(
        &mut self,
        q: &mut QuicRecords,
        authorize: impl FnMut() -> bool,
    ) -> Result<(), quic::Error> {
        if let Some(pending) = &mut self.pending {
            match pending.preflight(q, authorize) {
                Ok(Advance::Waiting | Advance::Ready) => {}
                Ok(Advance::PeerRetired) => {
                    self.pending = None;
                    self.reason.get_or_insert(Error::Cancelled);
                }
                Err(error) => {
                    self.reason.get_or_insert(Error::Transport(error));
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    pub(super) fn service(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), quic::Error> {
        if let Some(pending) = &mut self.pending {
            match pending.advance(q, &mut authorize) {
                Ok(Advance::Waiting) => return Ok(()),
                Ok(Advance::Ready) => {}
                Ok(Advance::PeerRetired) => {
                    // The transport retired the original reservation. Dropping
                    // this spent owner cannot abandon the desktop connection.
                    self.pending = None;
                    self.reason.get_or_insert(Error::Cancelled);
                    return Ok(());
                }
                Err(error) => {
                    self.reason.get_or_insert(Error::Transport(error));
                    // An unfinished ticket exchange has no independently retired
                    // lane yet. The normal session guard fences the parent.
                    return Err(error);
                }
            }
            let pending = self.pending.take().ok_or(quic::Error::Closed)?;
            let result = pending.start(q);
            match result {
                Ok(negotiation::Attached::Receiver(receiver)) => self.receiver = Some(*receiver),
                Ok(negotiation::Attached::Sender(sender)) => self.sender = Some(sender),
                Err(Error::Transport(error)) => {
                    self.reason.get_or_insert(Error::Transport(error));
                    return Err(error);
                }
                Err(error) => {
                    // Setup failed before a file owner was established. The peer
                    // may not have consumed its final attachment reply yet: never
                    // reset it while claiming independently successful teardown.
                    self.reason.get_or_insert(error);
                    return Err(quic::Error::Handler);
                }
            }
        }
        if let Some(outgoing) = &mut self.sender {
            if let Err(error) = outgoing.service(q, &mut authorize) {
                self.reason.get_or_insert(Error::Sender(error));
                if error == fr_files::sender::Error::WrongConnection || q.is_closed() {
                    return Err(quic::Error::Handler);
                }
                // Retire only the completed optional pair. The sender keeps
                // actual publication/unknown-effect receipts and source custody.
                outgoing
                    .sender
                    .cancel(q)
                    .map_err(|_| quic::Error::Handler)?;
            }
            return Ok(());
        }
        let Some(receiver) = &mut self.receiver else {
            return Ok(());
        };
        if let Err(error) = receiver.service(q, authorize) {
            // This owner retires only the optional file pair. A native transport
            // failure still reaches the parent through its ordinary I/O checks.
            self.reason.get_or_insert(Error::Receiver(error));
        }
        if receiver.state() == State::Retired {
            self.collect();
        }
        Ok(())
    }
    fn collect(&mut self) {
        let Some(receiver) = &mut self.receiver else {
            return;
        };
        if let Err(error) = receiver.collect_after_close() {
            self.reason.get_or_insert(Error::Receiver(error));
        }
        if self.cleanup.is_none() {
            self.cleanup = receiver.try_finish_cleanup();
            // A worker can publish just before its thread exits. Collect AFTER
            // the join as well: absence on the preceding poll proves nothing.
            if self.cleanup.is_some()
                && let Err(error) = receiver.collect_after_close()
            {
                self.reason.get_or_insert(Error::Receiver(error));
            }
        }
    }
    fn cleanup(&self) -> Cleanup {
        match (self.receiver.as_ref(), self.cleanup) {
            (None, _) => Cleanup::NotStarted,
            (_, Some(result)) => Cleanup::Finished(result),
            _ => Cleanup::Pending,
        }
    }
}
impl ControlledHost {
    /// Consume a separately completed file attachment on THIS controller. The
    /// destination is an already opened, locally approved directory handle; the
    /// peer cannot select a root. Permission is independent of desktop control.
    ///
    /// Call between normal `drive` turns. Those turns perform all subsequent
    /// bounded network service, including during admission refresh. No disk I/O
    /// or native input is performed on this owner. The slot is single-use even
    /// after retirement: a new slot requires a new authenticated controller.
    pub fn attach_files(
        &mut self,
        channel: MediaChannel,
        handle: u128,
        configuration: Configuration,
    ) -> Result<(), Error> {
        if self.files.upload.used
            || (self.files.download.used && !files::duplex_selected(&self.session.opened.selected))
        {
            return Err(Error::AlreadyAttached);
        }
        self.session.check().map_err(|_| Error::Closed)?;
        let selection = &self.session.opened.selected;
        if (selection.capabilities.iter().any(|cap| {
            cap.name == files::DOWNLOAD_CAPABILITY && cap.version == files::DOWNLOAD_VERSION
        }) && !files::duplex_selected(selection))
            || selection.role != Role::RequestControl
            || !selection
                .capabilities
                .iter()
                .any(|c| c.name == files::CAPABILITY && c.version == files::VERSION)
        {
            return Err(Error::NotNegotiated);
        }
        let q = &mut self.session.opened.transport;
        if channel.descriptor().role != fr_wire::attachment::MediaRole::Files
            || channel.completed_parent(q).map_err(Error::Transport)? != self.session.opened.binding
        {
            return Err(Error::WrongBinding);
        }
        let authority = self.input.file_authority(q).map_err(|_| Error::Closed)?;
        let lane = FilesChannel::new(q, channel, authority.binding().lease, handle)
            .map_err(Error::Transport)?;
        // The completed lane has reserved stream/transfer identity. Neither a
        // failed worker start nor retirement may silently create a second owner.
        self.files.upload.used = true;
        let result = HostReceiver::spawn_with_authority(
            self.session.opened.cx.clone(),
            q,
            lane,
            authority,
            configuration,
        );
        match result {
            Ok(receiver) => {
                self.files.upload.receiver = Some(receiver);
                Ok(())
            }
            Err(error) => {
                self.files.upload.reason = Some(Error::Receiver(error));
                Err(Error::Receiver(error))
            }
        }
    }
    /// The pending metadata exchange has not started a disk worker yet.
    pub fn file_receive_negotiating(&self) -> bool {
        self.files.upload.pending.is_some()
    }
    pub fn file_receive_state(&self) -> Option<State> {
        self.files.upload.receiver.as_ref().map(HostReceiver::state)
    }
    pub fn file_receive_progress(&self) -> Option<Progress> {
        self.files
            .upload
            .receiver
            .as_ref()
            .and_then(HostReceiver::progress)
    }
    /// The actual original disk result, still readable after retirement/closure.
    /// Missing results must never be interpreted as proof of no external effect.
    pub fn file_receive_result(&self) -> Option<ResultReceipt> {
        self.files
            .upload
            .receiver
            .as_ref()
            .and_then(HostReceiver::last_result)
    }
    pub fn file_receive_reason(&self) -> Option<Error> {
        self.files.upload.reason
    }
    /// Stop both optional file directions. Completed pairs retire independently;
    /// an unfinished exchange still requires the original parent fence.
    pub fn retire_files(&mut self) -> Result<(), Error> {
        let upload = self.retire_file_upload();
        let download = self.retire_file_download();
        upload.and(download)
    }
    /// Stop receipt into the host's selected drop directory only. A completed
    /// download continues on its independently admitted attachment.
    pub fn retire_file_upload(&mut self) -> Result<(), Error> {
        if self.files.upload.pending.is_some() {
            self.close();
            return Err(Error::Cancelled);
        }
        if let Some(receiver) = &mut self.files.upload.receiver {
            let result = receiver
                .retire(&mut self.session.opened.transport)
                .map_err(Error::Receiver);
            self.files.upload.collect();
            if let Err(error) = result {
                self.files.upload.reason.get_or_insert(error);
                return Err(error);
            }
        }
        Ok(())
    }
    /// Stop the host's finite selected source batch only. Publication receipts
    /// stay with this original sender; an upload receiver remains independent.
    pub fn retire_file_download(&mut self) -> Result<(), Error> {
        if self.files.download.pending.is_some() {
            self.close();
            return Err(Error::Cancelled);
        }
        if let Some(outgoing) = &mut self.files.download.sender {
            outgoing
                .sender
                .cancel(&mut self.session.opened.transport)
                .map_err(Error::Sender)?;
        }
        Ok(())
    }
    /// Nonblocking collection works after connection/session cancellation too.
    /// It never grants authority, retransmits a proof or restarts the disk task.
    pub fn file_receive_cleanup(&mut self) -> Cleanup {
        if self
            .files
            .upload
            .receiver
            .as_ref()
            .is_some_and(|r| r.state() != State::Active)
        {
            self.files.upload.collect();
        }
        self.files.upload.cleanup()
    }
    /// Drain BOTH original disk owners with an independently live cleanup
    /// context and the caller's original absolute deadline. No success is
    /// returned while either worker is pending; receipts survive every outcome.
    pub async fn reap_files(&mut self, cx: &Cx, deadline: Deadline) -> Result<Cleanup, Error> {
        self.retire_files()?;
        let mut previous = None;
        loop {
            cx.checkpoint().map_err(|_| Error::Cancelled)?;
            let current = cx.timer_driver().ok_or(Error::Clock)?.now();
            if previous.is_some_and(|before| current < before) {
                return Err(Error::Clock);
            }
            if current >= deadline.time() {
                return Err(Error::CleanupPending);
            }
            let upload = self.file_receive_cleanup();
            let download = match self.files.download.sender.as_mut() {
                Some(outgoing) => outgoing.sender.try_finish_cleanup(),
                None => Some(Ok(())),
            };
            if upload != Cleanup::Pending
                && let Some(result) = download
            {
                // Report a retained source failure only after the independent
                // receiver is joined too; neither result settles its sibling.
                result.map_err(Error::Sender)?;
                return Ok(match upload {
                    Cleanup::NotStarted if self.files.download.sender.is_some() => {
                        Cleanup::Finished(Ok(()))
                    }
                    outcome => outcome,
                });
            }
            previous = Some(current);
            sleep_until(
                Time::from_nanos(current.as_nanos().saturating_add(1_000_000)).min(deadline.time()),
            )
            .await;
        }
    }
}
