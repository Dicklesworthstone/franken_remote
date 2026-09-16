//! The dedicated clipboard pair, bound to one completed attachment and lease.
//! This is routing evidence, not an input grant or a native clipboard permission.
use super::{
    AttachedChannel, Cx, Disposition, Error, MediaChannel, Messages, Priority, QuicRecords, Route,
};
use fr_core::{
    clipboard::Binding,
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{
    attachment::MediaRole,
    clipboard::{self, Context, Lane, Role},
    negotiation::ControlBinding,
};
use std::cell::Cell;

/// Non-cloneable route owner. The caller retains the original controller owner
/// and separately granted native clipboard permission. Do not rebuild a receiver
/// or reset its sequence floor when a transfer expires. The parent session owns
/// all I/O, cancellation, and lifetime authorization, including during silence.
pub struct ClipboardChannel {
    attachment: MediaChannel,
    routes: AttachedChannel,
    parent: ControlBinding,
    outgoing: Context,
    incoming: Context,
    limits: ProtocolLimits,
}
impl std::fmt::Debug for ClipboardChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClipboardChannel([connection-bound controller lane])")
    }
}
impl Drop for ClipboardChannel {
    fn drop(&mut self) {
        self.attachment.close();
    }
}
impl ClipboardChannel {
    /// Consume the one-use completed clipboard proof. The lease is the already
    /// granted controller's immutable scope, never a value taken from clipboard
    /// records. Authority must still be checked at enqueue and OS publication.
    pub fn new(
        q: &QuicRecords,
        mut attachment: MediaChannel,
        scope: Binding,
    ) -> Result<Self, Error> {
        let setup = (|| {
            let routes = attachment.completed_on(q)?;
            let parent = attachment.completed_parent(q)?;
            let selected = attachment.completed_limits(q)?;
            if !scope.valid()
                || scope.session != parent.remote_session
                || routes.descriptor.role != MediaRole::Clipboard
                || routes.datagram.is_some()
                || routes.outbound.messages != Messages::Clipboard
                || routes.inbound.messages != Messages::Clipboard
                || routes.outbound.priority != Priority::Bulk
                || routes.inbound.priority != Priority::Bulk
            {
                return Err(Error::WrongRoute);
            }
            let maximum = u32::try_from(routes.byte_allowance).map_err(|_| Error::TooLarge)?;
            let caps = ProtocolLimits::with_overrides(LimitOverrides {
                max_control_message_bytes: Some(maximum),
                ..LimitOverrides::default()
            })
            .map_err(|_| Error::InvalidPolicy)?;
            // Only lower the actual lane's record cap; preserve every other
            // negotiated ceiling, in particular the one-MiB ITEM ceiling.
            let limits = selected.negotiated(&caps);
            let sender = if attachment.host {
                Role::Host
            } else {
                Role::Controller
            };
            let outgoing = Context {
                scope,
                channel: routes.descriptor.binding.parent.id,
                sender,
                lane: Lane::Clipboard,
            };
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
                let this = Self {
                    attachment,
                    routes,
                    parent,
                    outgoing,
                    incoming,
                    limits,
                };
                this.check(q)?;
                Ok(this)
            }
            Err(e) => {
                attachment.close();
                Err(e)
            }
        }
    }
    /// Route classification only, not authority or proof of connection identity.
    pub fn owns_inbound(&self, route: Route) -> bool {
        route == Route::Stream(self.routes.inbound)
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
    pub const fn limits(&self) -> ProtocolLimits {
        self.limits
    }
    /// Check the original object identity before touching its routes. A foreign
    /// connection with equal numeric IDs must not be closed or dispatched here.
    pub fn check(&self, q: &QuicRecords) -> Result<(), Error> {
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
    /// Invalidate the original attachment; its connection's next checked
    /// operation fences retained data. It never mutates an unrelated connection.
    pub fn close(&mut self) {
        self.attachment.close();
    }
    /// Gracefully retire only this completed clipboard pair on its ORIGINAL
    /// connection. Clears queued records, resets native send/retransmission state,
    /// and stops the peer direction. Control/input/media routes stay installed.
    /// Call between I/O turns; no OS call, wait, or replacement grant is involved.
    /// Drop without this explicit cleanup keeps its conservative connection fence.
    /// Bytes already delivered to the peer/OS cannot be recalled by a stream reset.
    pub fn retire(&mut self, q: &mut QuicRecords, cx: &Cx) -> Result<(), Error> {
        self.attachment.retire_clipboard(q, cx)
    }
    /// True only after the existing native empty-send witness has retired all
    /// queued, unsent and retransmittable bytes on this original route. This is
    /// not an application receipt. Keep the Egress permit until this is true.
    pub fn send_drained(&self, q: &QuicRecords) -> Result<bool, Error> {
        self.check(q)?;
        q.senders
            .iter()
            .find(|s| s.route == self.routes.outbound)
            .map(|s| s.records == 0 && s.bytes == 0)
            .ok_or(Error::WrongRoute)
    }
    /// Whole-record admission, not a partial socket write or publication receipt.
    /// The absolute transport-clock deadline comes from the ORIGINAL operation,
    /// not the time it finally obtained queue capacity. Recheck authorization in
    /// the session's drive callback too: an accepted record is still retained.
    pub fn send(
        &self,
        q: &mut QuicRecords,
        cx: &Cx,
        bytes: &[u8],
        deadline_us: u64,
        authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        self.check(q)?;
        clipboard::decode(bytes, self.outgoing, &self.limits).map_err(|_| Error::Malformed)?;
        q.send(
            cx,
            Route::Stream(self.routes.outbound),
            bytes,
            deadline_us,
            authorize,
        )
    }
    /// Dispatch at most one exact-lane record. The callback must be bounded and
    /// nonblocking; hand off to the native worker rather than call the OS here.
    /// Returning Blocked leaves the same complete record in transport ownership.
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
            |r| ready.get() && r == Route::Stream(self.routes.inbound),
            |_, bytes| {
                ready.set(false);
                clipboard::decode(bytes, self.incoming, &self.limits).map_err(|_| ())?;
                consume(bytes)
            },
        )?;
        self.check(q)
    }
}
