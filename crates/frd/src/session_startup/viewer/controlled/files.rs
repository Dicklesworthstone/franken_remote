//! One file sender carried by the original running controller, never a new grant.
use super::{ControlledViewer, permitted};
use asupersync::cx::Cx;
use fr_files::{
    sender::{Error, Policy, Progress, Receipt, Sender, Stage},
    session::Permission,
};
use fr_transport::quic::{MediaChannel, QuicRecords, Route, files::FilesChannel};
use fr_wire::{
    attachment,
    decoder::Binding,
    negotiation::{ControlBinding, Role},
};
use std::fs::File;

#[derive(Default)]
pub(super) struct Slot {
    sender: Option<Sender<'static>>,
    permission: Option<Permission>,
    failure: Option<Error>,
    stopped: bool,
}
impl Slot {
    pub(super) fn owns(&self, route: Route) -> bool {
        self.sender.as_ref().is_some_and(|s| s.owns_inbound(route))
    }
    pub(super) fn permits_io(&self) -> bool {
        self.stopped || self.permission.as_ref().is_none_or(Permission::is_approved)
    }
    pub(super) fn stop(&mut self, q: &mut QuicRecords) -> Result<(), Error> {
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
        authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
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
        if self.files.sender.is_some() || self.files.stopped {
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
        self.check().map_err(|_| Error::Closed)?;
        if !self.files.permits_io() || self.files.stopped {
            self.cancel_files()?;
            return Err(Error::Cancelled);
        }
        if !permitted(&mut self.input, &self.control, &self.session.cx) {
            return Err(Error::Closed);
        }
        self.files
            .sender
            .as_mut()
            .ok_or(Error::Closed)?
            .begin(&self.session.transport, file, name)
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
    /// Retire files only. Input, observation and their identities stay unchanged.
    pub fn cancel_files(&mut self) -> Result<(), Error> {
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
