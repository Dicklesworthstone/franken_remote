//! One-use file attachment on the ORIGINAL authenticated native connection.
//! This routing owner grants no file or input permission and resolves no paths.
use super::{
    AttachedChannel, Cx, Disposition, Error, MediaChannel, Messages, Priority, QuicRecords, Route,
};
use fr_core::ids::InputLeaseId;
use fr_wire::{
    attachment::MediaRole,
    files::{self, Context, Direction, Lane, Limits, Role},
    negotiation::ControlBinding,
};
use std::cell::Cell;

/// Controller-to-host file data and reverse acceptance/completion on one pair.
/// Non-cloneable and connection-identity checked, not just numerically bound.
pub struct FilesChannel {
    attachment: MediaChannel,
    connection: super::ConnectionBinding,
    routes: AttachedChannel,
    parent: ControlBinding,
    outgoing: Context,
    incoming: Context,
    limits: Limits,
}
impl std::fmt::Debug for FilesChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FilesChannel([original attachment])")
    }
}
impl FilesChannel {
    /// Consume a COMPLETED ticket exchange. Lease and handle come from local
    /// approval of the existing controller/destination, never an incoming offer.
    /// The receiver subsequently verifies these against its real input monitor.
    pub fn new(
        q: &QuicRecords,
        mut attachment: MediaChannel,
        lease: InputLeaseId,
        handle: u128,
    ) -> Result<Self, Error> {
        let setup = (|| {
            let routes = attachment.completed_on(q)?;
            let parent = attachment.completed_parent(q)?;
            let protocol = attachment.completed_limits(q)?;
            if routes.descriptor.role != MediaRole::Files
                || routes.datagram.is_some()
                || routes.outbound.messages != Messages::Files
                || routes.inbound.messages != Messages::Files
                || routes.outbound.priority != Priority::Bulk
                || routes.inbound.priority != Priority::Bulk
            {
                return Err(Error::WrongRoute);
            }
            let maximum = usize::try_from(routes.byte_allowance).map_err(|_| Error::TooLarge)?;
            let limits = Limits::new(&protocol, maximum).map_err(|_| Error::InvalidPolicy)?;
            let sender = if attachment.host {
                Role::Host
            } else {
                Role::Controller
            };
            let outgoing = Context {
                session: parent.remote_session,
                lease,
                handle,
                channel: routes.descriptor.binding.parent.id,
                sender,
                direction: Direction::ToHost,
                lane: Lane::Files,
            };
            outgoing.validate().map_err(|_| Error::WrongRoute)?;
            let incoming = Context {
                sender: if sender == Role::Host {
                    Role::Controller
                } else {
                    Role::Host
                },
                ..outgoing
            };
            Ok((routes, parent, outgoing, incoming, limits))
        })();
        match setup {
            Ok((routes, parent, outgoing, incoming, limits)) => {
                let channel = Self {
                    connection: q.binding(),
                    attachment,
                    routes,
                    parent,
                    outgoing,
                    incoming,
                    limits,
                };
                channel.check(q)?;
                Ok(channel)
            }
            Err(error) => {
                attachment.close();
                Err(error)
            }
        }
    }
    pub const fn parent(&self) -> ControlBinding {
        self.parent
    }
    pub const fn outgoing(&self) -> Context {
        self.outgoing
    }
    pub const fn incoming(&self) -> Context {
        self.incoming
    }
    pub const fn limits(&self) -> Limits {
        self.limits
    }
    pub fn owns_inbound(&self, route: Route) -> bool {
        route == Route::Stream(self.routes.inbound)
    }
    /// Foreign objects cannot be sent to, dispatched or retired even with equal IDs.
    pub fn check(&self, q: &QuicRecords) -> Result<(), Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(Error::WrongRoute);
        }
        if q.is_closed() || !self.attachment.is_complete() {
            return Err(Error::Closed);
        }
        if self.attachment.completed_on(q)? != self.routes
            || !q.has_route(Route::Stream(self.routes.outbound))
            || !q.has_route(Route::Stream(self.routes.inbound))
            || q.receive_ended(self.routes.inbound)?
            || q.receive_ended(self.attachment.control.inbound)?
        {
            return Err(Error::Closed);
        }
        Ok(())
    }
    /// Removes only file send/receive state; never renews input or recycles IDs.
    pub fn retire(&mut self, q: &mut QuicRecords, cx: &Cx) -> Result<(), Error> {
        self.attachment.retire_files(q, cx)
    }
    pub fn send_drained(&self, q: &QuicRecords) -> Result<bool, Error> {
        self.check(q)?;
        q.senders
            .iter()
            .find(|s| s.route == self.routes.outbound)
            .map(|s| s.records == 0 && s.bytes == 0)
            .ok_or(Error::WrongRoute)
    }
    /// Queue once under the operation's original deadline, never a refreshed
    /// deadline on backpressure. This receipt is not filesystem publication.
    pub fn send(
        &self,
        q: &mut QuicRecords,
        cx: &Cx,
        bytes: &[u8],
        deadline_us: u64,
        authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        self.check(q)?;
        files::decode(bytes, self.outgoing, self.limits).map_err(|_| Error::Malformed)?;
        q.send(
            cx,
            Route::Stream(self.routes.outbound),
            bytes,
            deadline_us,
            authorize,
        )
    }
    /// At most one record per turn; Blocked retains the SAME transport-owned
    /// record while other streams remain available to the session's dispatcher.
    pub fn dispatch(
        &self,
        q: &mut QuicRecords,
        cx: &Cx,
        authorize: impl FnMut() -> bool,
        mut consume: impl FnMut(&[u8]) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        self.check(q)?;
        let ready = Cell::new(true);
        q.receive_ready(
            cx,
            authorize,
            |route| ready.get() && self.owns_inbound(route),
            |_, bytes| {
                ready.set(false);
                files::decode(bytes, self.incoming, self.limits).map_err(|_| ())?;
                consume(bytes)
            },
        )?;
        self.check(q)
    }
}
impl Drop for FilesChannel {
    fn drop(&mut self) {
        // An abandoned attachment is not graceful retirement. Preserve the same
        // conservative connection fence as the other role-specific route owners.
        self.attachment.close();
    }
}
