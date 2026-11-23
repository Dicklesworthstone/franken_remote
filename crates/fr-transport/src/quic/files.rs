//! One-use file attachment on the ORIGINAL authenticated native connection.
//! This routing owner grants no file or input permission and resolves no paths.
use super::{
    AttachedChannel, Cx, Disposition, Error, MediaChannel, Messages, Priority, QuicRecords, Route,
};
use fr_core::ids::InputLeaseId;
use fr_wire::{
    attachment::MediaRole,
    files::{self, Context, Lane, Limits, Role},
    negotiation::ControlBinding,
    sync_job_state::{self, SyncJobState},
};
use std::cell::Cell;

/// Positively selected file direction and reverse receipts on one original pair.
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
            let messages = attachment.application_messages(attachment.host);
            if !routes.descriptor.role.is_files()
                || (routes.descriptor.role == MediaRole::FilesDownload
                    && attachment.files_direction != files::Direction::ToController)
                || routes.datagram.is_some()
                || !matches!(messages, Messages::Files | Messages::FilesSync)
                || routes.outbound.messages != messages
                || routes.inbound.messages != messages
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
                direction: attachment.files_direction,
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
    /// This original download attachment positively selected bounded job state.
    /// Local job configuration, acceptance and filesystem authority remain the
    /// file owner's responsibility; ordinary file consent is not sync consent.
    pub fn sync_selected(&self) -> bool {
        self.routes.outbound.messages == Messages::FilesSync
            && self.outgoing.direction == files::Direction::ToController
    }
    pub fn owns_inbound(&self, route: Route) -> bool {
        route == Route::Stream(self.routes.inbound)
    }
    /// Foreign objects cannot be sent to, dispatched or retired even with equal IDs.
    pub fn check(&self, q: &QuicRecords) -> Result<(), Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(Error::WrongRoute);
        }
        if q.is_closed() {
            return Err(Error::Closed);
        }
        if self.receive_expired(q) {
            return Err(Error::Stream(fr_wire::stream::StreamError::Expired));
        }
        if self.send_expired(q) {
            return Err(Error::SendExpired);
        }
        if !self.attachment.is_complete() {
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
    /// This original completed pair expired at its fixed first-byte receive
    /// deadline, locally or as reported by the peer's file-expiry reset code.
    /// Both stalled prefixes and unconsumed records are terminal to this
    /// transfer; no expired bytes are dispatched or retried.
    /// Closed or foreign connections cannot supply this isolation evidence.
    pub fn receive_expired(&self, q: &QuicRecords) -> bool {
        self.attachment.receive_expired(q)
    }
    /// The original completed pair retired after a retained send exceeded its
    /// immutable delivery deadline, locally or as reported by its peer. This
    /// does not establish whether earlier file data was published. A foreign
    /// or closed connection cannot supply optional-retirement evidence.
    pub fn send_expired(&self, q: &QuicRecords) -> bool {
        self.attachment.send_expired(q)
    }
    /// Request retirement of this original completed pair without borrowing its
    /// connection. The next checked operation retires it before new application
    /// work. During native I/O, only an immutable payload-empty entry proof can
    /// preserve the parent; retained or uncertain payload still fences it.
    /// Keep this owner until ordinary retirement/cleanup can finish.
    pub fn request_retirement(&self) {
        self.attachment.request_retirement();
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
    /// Application records still owned by this exact outbound lane, including
    /// queued and partially staged records. The original attachment prefix
    /// remains charged to transport but consumes no file payload permit.
    /// The transport removes only a fully acknowledged prefix; an exclusive
    /// sender can retire the same prefix of its permits without waiting for
    /// later records. A reset, expired, closed or foreign lane returns an error,
    /// never evidence of delivery or file publication.
    pub fn retained_send_records(&self, q: &QuicRecords) -> Result<usize, Error> {
        self.check(q)?;
        let sender = q
            .senders
            .iter()
            .find(|sender| sender.route == self.routes.outbound)
            .ok_or(Error::WrongRoute)?;
        sender
            .records
            .checked_sub(sender.setup_records)
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
        self.validate_record(bytes, self.outgoing)?;
        let result = q.send(
            cx,
            Route::Stream(self.routes.outbound),
            bytes,
            deadline_us,
            authorize,
        );
        // The checked send can retire the original pair before admitting this
        // new record. Preserve that terminal cause instead of reporting the
        // subsequently missing route or this new record's admission deadline.
        match result {
            Err(Error::WrongRoute | Error::Expired) if self.send_expired(q) => {
                Err(Error::SendExpired)
            }
            other => other,
        }
    }
    /// Send one canonical state under its original deadline and the SAME file
    /// queue accounting as ATP records. The exclusive file owner must wait for
    /// its earlier payload/proof custody to drain before using this operation;
    /// this does not create a separate queue or an uncharged metadata prefix.
    pub fn send_sync(
        &self,
        q: &mut QuicRecords,
        cx: &Cx,
        state: &SyncJobState,
        deadline_us: u64,
        authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        self.check(q)?;
        if !self.sync_selected() {
            return Err(Error::WrongRoute);
        }
        let mut bytes = [0; sync_job_state::RECORD_BYTES];
        let length = sync_job_state::encode(state, self.outgoing, self.limits, &mut bytes)
            .map_err(|_| Error::Malformed)?;
        self.send(q, cx, &bytes[..length], deadline_us, authorize)
    }

    fn validate_record(&self, bytes: &[u8], context: Context) -> Result<(), Error> {
        if files::decode(bytes, context, self.limits).is_ok()
            || (self.sync_selected() && sync_job_state::decode(bytes, context, self.limits).is_ok())
        {
            Ok(())
        } else {
            Err(Error::Malformed)
        }
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
                self.validate_record(bytes, self.incoming).map_err(|_| ())?;
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
