//! Optional ATP receipt owned by the original running controller.
//!
//! Disk work stays in fr-files' single bounded worker. The regular control turn
//! services this lane only after input and renewal; no application callback sees
//! its records. Retirement never erases an already committed publication.
mod negotiation;
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
use negotiation::Pending;

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
    }
    pub(super) fn owns(&self, route: Route, bytes: &[u8]) -> bool {
        self.pending.as_ref().is_some_and(|p| p.owns(route, bytes))
            || self
                .receiver
                .as_ref()
                .is_some_and(|r| r.owns_inbound(route))
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
    pub(super) fn service(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), quic::Error> {
        if let Some(pending) = &mut self.pending {
            match pending.advance(q, &mut authorize) {
                Ok(false) => return Ok(()),
                Ok(true) => {}
                Err(error) => {
                    self.reason.get_or_insert(Error::Transport(error));
                    // An unfinished ticket exchange has no independently retired
                    // lane yet. The normal session guard fences the parent.
                    return Err(error);
                }
            }
            let pending = self.pending.take().ok_or(quic::Error::Closed)?;
            let result = pending.start_receiver(q);
            match result {
                Ok(receiver) => self.receiver = Some(receiver),
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
        if self.files.used {
            return Err(Error::AlreadyAttached);
        }
        self.session.check().map_err(|_| Error::Closed)?;
        let selection = &self.session.opened.selected;
        if selection.role != Role::RequestControl
            || !selection
                .capabilities
                .iter()
                .any(|c| c.name == files::CAPABILITY && c.version == files::VERSION)
        {
            return Err(Error::NotNegotiated);
        }
        let q = &mut self.session.opened.transport;
        if channel.completed_parent(q).map_err(Error::Transport)? != self.session.opened.binding {
            return Err(Error::WrongBinding);
        }
        let authority = self.input.file_authority(q).map_err(|_| Error::Closed)?;
        let lane = FilesChannel::new(q, channel, authority.binding().lease, handle)
            .map_err(Error::Transport)?;
        // The completed lane has reserved stream/transfer identity. Neither a
        // failed worker start nor retirement may silently create a second owner.
        self.files.used = true;
        let result = HostReceiver::spawn_with_authority(
            self.session.opened.cx.clone(),
            q,
            lane,
            authority,
            configuration,
        );
        match result {
            Ok(receiver) => {
                self.files.receiver = Some(receiver);
                Ok(())
            }
            Err(error) => {
                self.files.reason = Some(Error::Receiver(error));
                Err(Error::Receiver(error))
            }
        }
    }
    /// The pending metadata exchange has not started a disk worker yet.
    pub fn file_receive_negotiating(&self) -> bool {
        self.files.pending.is_some()
    }
    pub fn file_receive_state(&self) -> Option<State> {
        self.files.receiver.as_ref().map(HostReceiver::state)
    }
    pub fn file_receive_progress(&self) -> Option<Progress> {
        self.files
            .receiver
            .as_ref()
            .and_then(HostReceiver::progress)
    }
    /// The actual original disk result, still readable after retirement/closure.
    /// Missing results must never be interpreted as proof of no external effect.
    pub fn file_receive_result(&self) -> Option<ResultReceipt> {
        self.files
            .receiver
            .as_ref()
            .and_then(HostReceiver::last_result)
    }
    pub fn file_receive_reason(&self) -> Option<Error> {
        self.files.reason
    }
    /// Stop and reset only file streams. The controller and its renewal remain
    /// live. Keep driving or call `reap_files` to observe original disk cleanup.
    /// Cancelling an unfinished ticket exchange instead fences the parent: there
    /// is not yet a completed optional pair that can be reset independently.
    pub fn retire_files(&mut self) -> Result<(), Error> {
        if self.files.pending.is_some() {
            self.close();
            return Err(Error::Cancelled);
        }
        if let Some(receiver) = &mut self.files.receiver {
            let result = receiver
                .retire(&mut self.session.opened.transport)
                .map_err(Error::Receiver);
            self.files.collect();
            if let Err(error) = result {
                self.files.reason.get_or_insert(error);
                return Err(error);
            }
        }
        Ok(())
    }
    /// Nonblocking collection works after connection/session cancellation too.
    /// It never grants authority, retransmits a proof or restarts the disk task.
    pub fn file_receive_cleanup(&mut self) -> Cleanup {
        if self
            .files
            .receiver
            .as_ref()
            .is_some_and(|r| r.state() != State::Active)
        {
            self.files.collect();
        }
        self.files.cleanup()
    }
    /// Drain with an independently live cleanup context and the caller's ORIGINAL
    /// absolute deadline. Cancellation/expiry retain custody and actual receipts
    /// for a later collection; they never label the worker clean or replay work.
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
            let result = self.file_receive_cleanup();
            if result != Cleanup::Pending {
                return Ok(result);
            }
            previous = Some(current);
            sleep_until(
                Time::from_nanos(current.as_nanos().saturating_add(1_000_000)).min(deadline.time()),
            )
            .await;
        }
    }
}
