//! One file sender carried by the original running controller, never a new grant.
mod negotiation;
use super::{ControlledViewer, permitted};
use asupersync::cx::Cx;
use fr_files::{
    sender::{
        Error, Policy, Progress, Receipt, Sender, Stage,
        batch::{Report, Selection},
    },
    session::Permission,
};
use fr_transport::quic::{MediaChannel, QuicRecords, Route, files::FilesChannel};
use fr_wire::{
    attachment,
    decoder::Binding,
    negotiation::{ControlBinding, Role},
};
use negotiation::Pending;
use std::{fs::File, time::Duration};

#[derive(Default)]
pub(super) struct Slot {
    used: bool,
    pending: Option<Pending>,
    sender: Option<Sender<'static>>,
    permission: Option<Permission>,
    failure: Option<Error>,
    stopped: bool,
}
impl Slot {
    pub(super) fn owns(&self, route: Route, bytes: &[u8]) -> bool {
        self.pending.as_ref().is_some_and(|p| p.owns(route, bytes))
            || self.sender.as_ref().is_some_and(|s| s.owns_inbound(route))
    }
    pub(super) fn deadline_us(&self) -> Option<u64> {
        self.pending.as_ref().map(Pending::deadline)
    }
    pub(super) fn check_pending(&mut self) -> Result<(), Error> {
        if let Some(pending) = &mut self.pending
            && let Err(error) = pending.check()
        {
            self.failure.get_or_insert(error);
            return Err(error);
        }
        Ok(())
    }
    pub(super) fn permits_io(&mut self) -> bool {
        self.check_pending().is_ok()
            && (self.stopped || self.permission.as_ref().is_none_or(Permission::is_approved))
    }
    pub(super) fn stop(&mut self, q: &mut QuicRecords) -> Result<(), Error> {
        if self.pending.take().is_some() {
            self.failure.get_or_insert(Error::Cancelled);
        }
        if !self.stopped {
            self.stopped = true;
            if let Some(sender) = &mut self.sender {
                sender.cancel(q)?;
            }
        }
        Ok(())
    }
    pub(super) fn service(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        if let Some(pending) = &mut self.pending {
            match pending.advance(q, &mut authorize) {
                Ok(false) => return Ok(()),
                Ok(true) => {}
                Err(error) => {
                    self.failure.get_or_insert(error);
                    // The peer may still own its final handshake response. An
                    // incomplete attachment cannot be retired as a file-only lane.
                    return Err(error);
                }
            }
            match self.pending.take().ok_or(Error::Closed)?.start(q) {
                Ok((sender, permission)) => {
                    self.sender = Some(sender);
                    self.permission = Some(permission);
                }
                Err(error) => {
                    self.failure.get_or_insert(error);
                    return Err(error);
                }
            }
        }
        if !self.permits_io() {
            self.failure.get_or_insert(Error::Cancelled);
            return self.stop(q);
        }
        if self.stopped {
            return Ok(());
        }
        if let Some(sender) = &mut self.sender {
            let permission = self.permission.as_ref().ok_or(Error::Closed)?;
            let mut authorize = authorize;
            if let Err(error) = sender.service(q, || permission.is_approved() && authorize()) {
                self.failure.get_or_insert(error);
                // Local source/proof failure is terminal for files, not for desktop
                // input. Sender retires the pair and preserves its real receipt.
                // A substituted/closed connection still fails the containing owner.
                if error == Error::WrongConnection || q.is_closed() {
                    return Err(error);
                }
                sender.cancel(q)?;
                self.stopped = true;
            }
        }
        Ok(())
    }
}
impl ControlledViewer {
    /// Attach one completed file pair to THIS decoder-backed controller. `handle`
    /// is the previously agreed file scope, not a path. `permission` is separate
    /// local consent; a read-only session or unnegotiated capability cannot join.
    /// No file is opened/read here. The explicit descriptor passed to `send_file`
    /// is the only possible source, and only ordinary controller turns send it.
    pub fn attach_files(
        &mut self,
        channel: MediaChannel,
        handle: u128,
        permission: Permission,
        policy: Policy,
    ) -> Result<(), Error> {
        if self.files.used || self.files.stopped {
            return Err(Error::Busy);
        }
        self.files_admitted(&permission)?;
        let q = &self.session.transport;
        let expected = Binding {
            parent: ControlBinding {
                id: channel.descriptor().binding.parent.id,
                ..self.session.opened.binding
            },
            ..self.media.binding()
        };
        if channel.completed_parent(q).map_err(Error::Transport)? != self.session.opened.binding
            || channel.descriptor().binding != expected
        {
            return Err(Error::WrongConnection);
        }
        let lane = FilesChannel::new(q, channel, self.input.binding().lease, handle)
            .map_err(Error::Transport)?;
        // The completed attachment has spent this single-use channel identity,
        // even if local sender configuration is subsequently refused.
        self.files.used = true;
        let sender = Sender::owning(self.session.cx.clone(), q, lane, policy)?;
        self.files.sender = Some(sender);
        self.files.permission = Some(permission);
        Ok(())
    }
    fn files_admitted(&mut self, permission: &Permission) -> Result<(), Error> {
        self.check().map_err(|_| Error::Closed)?;
        let selection = &self.session.opened.selection;
        if selection.role != Role::RequestControl
            || ![
                (attachment::CAPABILITY, attachment::VERSION),
                (attachment::FILES_CAPABILITY, attachment::FILES_VERSION),
                (fr_wire::files::CAPABILITY, fr_wire::files::VERSION),
            ]
            .iter()
            .all(|(name, version)| {
                selection
                    .capabilities
                    .iter()
                    .any(|c| c.name == *name && c.version == *version)
            })
        {
            return Err(Error::WrongRole);
        }
        if !permission.is_approved() || !permitted(&mut self.input, &self.control, &self.session.cx)
        {
            return Err(Error::Cancelled);
        }
        Ok(())
    }
    /// Begin one explicitly selected local descriptor. Names/content never enter
    /// diagnostics; preparation uses the existing bounded disk worker. Results and
    /// source cleanup must be collected before a subsequent file can be started.
    pub fn send_file(&mut self, file: File, name: &str) -> Result<u64, Error> {
        self.admit_file_send()?;
        self.files
            .sender
            .as_mut()
            .ok_or(Error::Closed)?
            .begin(&self.session.transport, file, name)
    }
    /// Queue a finite local multi-selection on this same authenticated file lane.
    /// No source is read until an ordinary controller turn rechecks permission,
    /// input authority and view freshness. The absolute lifetime includes waiting,
    /// hashing, sending, remote proof and source cleanup; it is never renewed by
    /// input lease renewal. Sources are processed one at a time in selection order.
    ///
    /// The caller must collect the final batch report before submitting another
    /// selection or single file. A failure stops the remaining sources, preserving
    /// prior publication receipts. This is not an automatically replayed sync job.
    pub fn send_files(&mut self, selection: Selection, lifetime: Duration) -> Result<(), Error> {
        self.admit_file_send()?;
        self.files
            .sender
            .as_mut()
            .ok_or(Error::Closed)?
            .begin_batch(&self.session.transport, selection, lifetime)
    }
    fn admit_file_send(&mut self) -> Result<(), Error> {
        self.check().map_err(|_| Error::Closed)?;
        if self.files.pending.is_some() {
            return Err(Error::Busy);
        }
        if !self.files.permits_io() || self.files.stopped {
            self.cancel_files()?;
            return Err(Error::Cancelled);
        }
        if !permitted(&mut self.input, &self.control, &self.session.cx) {
            return Err(Error::Closed);
        }
        Ok(())
    }
    /// Content-free, ordered results, including after permission or parent loss.
    /// Queued sources are not counted as started, and publication uncertainty is
    /// never reported as a rollback or used as a reason to resubmit a selection.
    pub fn file_batch_report(&self) -> Option<Report> {
        self.files.sender.as_ref().and_then(Sender::batch_report)
    }
    /// Collect only a complete report after the original source has been joined.
    /// This remains usable after close and cannot reopen a retired file lane.
    pub fn take_file_batch_report(&mut self) -> Option<Report> {
        self.files
            .sender
            .as_mut()
            .and_then(Sender::take_batch_report)
    }
    pub fn file_stage(&self) -> Option<Stage> {
        self.files.sender.as_ref().map(Sender::stage)
    }
    pub fn file_progress(&self) -> Option<Progress> {
        self.files.sender.as_ref().and_then(Sender::progress)
    }
    pub fn file_result(&self) -> Option<Receipt> {
        self.files.sender.as_ref().and_then(Sender::result)
    }
    pub fn file_failure(&self) -> Option<Error> {
        self.files.failure
    }
    /// This remains available after controller shutdown and never replays a file.
    pub fn take_file_result(&mut self) -> Option<Receipt> {
        self.files.sender.as_mut().and_then(Sender::take_result)
    }
    pub fn file_cleanup_finished(&self) -> bool {
        self.files
            .sender
            .as_ref()
            .is_none_or(Sender::cleanup_finished)
    }
    /// A completed file lane retires independently. An unfinished attachment
    /// instead fences the parent: a partially acknowledged pair is not clean.
    pub fn cancel_files(&mut self) -> Result<(), Error> {
        if self.files.pending.is_some() {
            self.close();
            return Err(Error::Cancelled);
        }
        self.files.stop(&mut self.session.transport)
    }
    /// Stop files, then join the already-finished original disk source using a separate
    /// cleanup context. Expiry/cancellation/drop keep the sender and receipt here;
    /// there is no detached replacement, renewed deadline or blocking thread join.
    pub async fn reap_files(
        &mut self,
        cleanup: &Cx,
        deadline: crate::worker::Deadline,
    ) -> Result<(), Error> {
        self.cancel_files()?;
        let clock = cleanup.timer_driver().ok_or(Error::Clock)?;
        let mut previous = clock.now();
        loop {
            cleanup.checkpoint().map_err(|_| Error::Cancelled)?;
            let now = clock.now();
            if now < previous {
                return Err(Error::Clock);
            }
            if now >= deadline.time() {
                return Err(Error::Expired);
            }
            previous = now;
            if let Some(result) = self
                .files
                .sender
                .as_mut()
                .map_or(Some(Ok(())), Sender::try_finish_cleanup)
            {
                if let Err(error) = result {
                    self.files.failure.get_or_insert(error);
                }
                return result;
            }
            asupersync::time::sleep_until(
                asupersync::types::Time::from_nanos(now.as_nanos().saturating_add(1_000_000))
                    .min(deadline.time()),
            )
            .await;
        }
    }
    pub(super) fn service_files(&mut self) -> Result<(), super::Error> {
        self.files
            .service(&mut self.session.transport, || {
                permitted(&mut self.input, &self.control, &self.session.cx)
            })
            .map_err(super::Error::Files)
    }
}
