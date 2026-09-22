//! Host file receipt tied to an existing native controller, never a new grant.
//!
//! This synchronous owner belongs on a disk worker. Clock callbacks are trusted
//! local monotonic reads in the original host's domain, never wire timestamps.
//! There is one active object, bounded shared disk reservations, and no payload
//! queue. Transport attachment and positive capability negotiation are separate.
use crate::receive::{self, DropDirectory, MAX_CHUNK_BYTES, PendingObject, Publication};
use asupersync::atp::{object::ContentId, safety::validate_portable_path_component};
use fr_core::{
    ids::{InputLeaseId, InputTicketId, RemoteSessionId},
    input_submission::{InputMonitor, InputSession, Refusal},
    time::{HostDuration, HostInstant},
};
use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const RECORD_COST: u64 = 128;
const MICRO: u128 = 1_000_000;

/// A separately negotiated AND locally approved receive capability. A clone
/// can only revoke this permission, not re-enable it or grant input authority.
/// Create a fresh permission only after a fresh local admission decision.
#[derive(Clone, Default)]
pub struct Permission(Arc<AtomicBool>);
impl Permission {
    pub fn new(approved: bool) -> Self {
        Self(Arc::new(AtomicBool::new(approved)))
    }
    pub fn revoke(&self) {
        self.0.store(false, Ordering::Release);
    }
    pub fn is_approved(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
impl fmt::Debug for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FilePermission")
            .field(&self.is_approved())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub session: RemoteSessionId,
    pub lease: InputLeaseId,
}

/// Read-only handoff from the original native input owner to its file worker.
/// Only a real `InputSession` can create this value. Clones retain the same
/// opaque native-owner identity; numeric session/lease values are not authority.
#[derive(Clone)]
pub struct Authority {
    monitor: InputMonitor,
    binding: Binding,
}
impl fmt::Debug for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FileAuthority([original input owner])")
    }
}
impl Authority {
    pub fn from_input(input: &InputSession) -> Self {
        let scope = input.ticket_credentials(InputTicketId::from_raw(0));
        Self {
            monitor: input.monitor(),
            binding: Binding {
                session: scope.session,
                lease: scope.lease,
            },
        }
    }
    pub fn binding(&self) -> Binding {
        self.binding
    }
    pub fn deadline(&self, now: HostInstant) -> Result<HostInstant, Refusal> {
        self.monitor.deadline(now)
    }
}

/// Transfer IDs strictly increase within this original lane, even after cancel.
/// Names and content identities are intentionally absent from diagnostics.
pub struct Offer<'a> {
    pub binding: Binding,
    pub id: u64,
    pub name: &'a str,
    pub size: u64,
    pub content: ContentId,
}
impl fmt::Debug for Offer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FileOffer([private metadata])")
    }
}

/// A token bucket charges payload PLUS 128 bytes per operation (plus the offer
/// name). Metadata-only offers cannot bypass rate admission. The transfer's
/// absolute time budget never slides when chunks arrive or authority renews.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub bytes_per_second: u32,
    pub burst_bytes: u32,
    pub transfer_lifetime: HostDuration,
    /// Cumulative declarations, not just currently reserved staging bytes.
    pub max_session_bytes: u64,
    /// Includes admitted attempts that later fail or are cancelled; zero-byte
    /// objects cannot bypass a finite metadata/disk-operation budget.
    pub max_session_transfers: u32,
}
impl Policy {
    pub fn conservative() -> Self {
        Self {
            bytes_per_second: 8 * 1024 * 1024,
            burst_bytes: 2 * (65_536 + 128),
            transfer_lifetime: HostDuration::from_micros(1_800_000_000),
            max_session_bytes: 16 * 1024 * 1024 * 1024,
            max_session_transfers: 1024,
        }
    }
    fn validate(self) -> Result<Self, Error> {
        let minimum = MAX_CHUNK_BYTES as u64 + RECORD_COST;
        if self.bytes_per_second == 0
            || !(minimum..=4 * minimum).contains(&u64::from(self.burst_bytes))
            || self.transfer_lifetime.as_micros() == 0
            || self.transfer_lifetime.as_micros() > 3_600_000_000
            || self.max_session_bytes == 0
            || self.max_session_transfers == 0
        {
            return Err(Error::Policy);
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Protocol,
    UnknownEffect,
    Policy,
    Permission,
    WrongBinding,
    Sequence,
    Busy,
    NoTransfer,
    RateLimited,
    Quota,
    Clock,
    Expired,
    Closed,
    Authority(Refusal),
    Storage(receive::Error),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "file-session: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub id: u64,
    pub staged_bytes: u64,
    pub total_bytes: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receipt {
    pub id: u64,
    pub bytes: u64,
    pub publication: Publication,
}
/// Non-refundable session admission counters; no path, name or content hash.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub transfers: u32,
    pub declared_bytes: u64,
}

pub(crate) struct VerifiedOffer<'a> {
    pub binding: Binding,
    pub id: u64,
    pub name: &'a str,
    pub size: u64,
    pub expected: receive::Expected,
}

struct Active {
    id: u64,
    size: u64,
    deadline: HostInstant,
    file: PendingObject,
}
impl Active {
    fn progress(&self) -> Progress {
        Progress {
            id: self.id,
            staged_bytes: self.file.received_bytes(),
            total_bytes: self.size,
        }
    }
}

/// Original host controller + separately approved local drop directory.
/// Construction requires a real `InputSession`; a view-only session has no such
/// owner. This monitor checks opaque native-owner identity as well as IDs, so
/// dropping/replacing the controller cannot resurrect old transfers.
/// Exactly one instance must be owned by the original negotiated file lane.
pub struct HostReceiver {
    directory: DropDirectory,
    authority: InputMonitor,
    binding: Binding,
    permission: Permission,
    policy: Policy,
    usage: Usage,
    last_time: HostInstant,
    tokens: u128,
    floor: Option<u64>,
    active: Option<Active>,
    closed: bool,
}
impl fmt::Debug for HostReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostFileReceiver")
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}
impl HostReceiver {
    pub fn new(
        input: &InputSession,
        directory: DropDirectory,
        permission: Permission,
        policy: Policy,
        now: HostInstant,
    ) -> Result<Self, Error> {
        Self::with_authority(
            Authority::from_input(input),
            directory,
            permission,
            policy,
            now,
        )
    }
    /// Continue after the input session has moved to the native worker. This
    /// accepts only its original read-only handoff, never caller-authored IDs.
    pub fn with_authority(
        authority: Authority,
        directory: DropDirectory,
        permission: Permission,
        policy: Policy,
        now: HostInstant,
    ) -> Result<Self, Error> {
        let policy = policy.validate()?;
        let mut result = Self {
            directory,
            binding: authority.binding,
            authority: authority.monitor,
            permission,
            policy,
            usage: Usage::default(),
            last_time: now,
            tokens: u128::from(policy.burst_bytes) * MICRO,
            floor: None,
            active: None,
            closed: false,
        };
        result.check(now)?;
        Ok(result)
    }
    pub fn binding(&self) -> Binding {
        self.binding
    }
    pub fn usage(&self) -> Usage {
        self.usage
    }
    pub fn progress(&self) -> Option<Progress> {
        self.active.as_ref().map(Active::progress)
    }
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// No byte of a file is accepted before original control and file permission
    /// pass. Checks again after potentially slow staging-file creation.
    pub fn begin(
        &mut self,
        offer: Offer<'_>,
        clock: impl FnMut() -> HostInstant,
    ) -> Result<Progress, Error> {
        self.begin_verified(
            VerifiedOffer {
                binding: offer.binding,
                id: offer.id,
                name: offer.name,
                size: offer.size,
                expected: receive::Expected::Content(offer.content),
            },
            clock,
        )
    }

    pub(crate) fn begin_verified(
        &mut self,
        offer: VerifiedOffer<'_>,
        mut clock: impl FnMut() -> HostInstant,
    ) -> Result<Progress, Error> {
        self.binding_check(offer.binding)?;
        let now = clock();
        self.check(now)?;
        if self.active.is_some() {
            return Err(Error::Busy);
        }
        if self.floor.is_some_and(|floor| offer.id <= floor) {
            return Err(Error::Sequence);
        }
        if offer.name.len() > 255
            || offer.name.starts_with(".fr-part-")
            || validate_portable_path_component(offer.name).is_err()
        {
            return Err(Error::Storage(receive::Error::InvalidName));
        }
        let declared_bytes = self
            .usage
            .declared_bytes
            .checked_add(offer.size)
            .ok_or(Error::Quota)?;
        if self.usage.transfers >= self.policy.max_session_transfers
            || declared_bytes > self.policy.max_session_bytes
        {
            return Err(Error::Quota);
        }
        let metadata_bytes = match &offer.expected {
            receive::Expected::Directory(manifest) => manifest.metadata_bytes(),
            _ => 0,
        };
        self.charge(RECORD_COST + offer.name.len() as u64 + metadata_bytes)?;
        let deadline = now
            .checked_add(self.policy.transfer_lifetime)
            .ok_or(Error::Clock)?;
        self.floor = Some(offer.id);
        // Never refund on cancellation, conflict, write failure or publication:
        // releasing a temporary reservation is not permission for unlimited disk.
        self.usage.transfers += 1;
        self.usage.declared_bytes = declared_bytes;
        let file = self
            .directory
            .begin_object(offer.name, offer.size, offer.expected)
            .map_err(Error::Storage)?;
        self.active = Some(Active {
            id: offer.id,
            size: offer.size,
            deadline,
            file,
        });
        if let Err(error) = self.check(clock()) {
            // No destination exists; failed post-create admission cannot retain
            // a disk reservation indefinitely in an otherwise idle lane.
            self.active.take();
            return Err(error);
        }
        Ok(self.progress().expect("new active transfer"))
    }

    /// Rate refusal happens before disk I/O and does not consume the offset.
    /// A disk failure is terminal for this object; it is never replayed.
    pub fn write(
        &mut self,
        binding: Binding,
        id: u64,
        offset: u64,
        bytes: &[u8],
        clock: impl FnMut() -> HostInstant,
    ) -> Result<Progress, Error> {
        self.write_entry(binding, id, 0, offset, bytes, clock)
    }
    pub fn write_entry(
        &mut self,
        binding: Binding,
        id: u64,
        index: u32,
        offset: u64,
        bytes: &[u8],
        mut clock: impl FnMut() -> HostInstant,
    ) -> Result<Progress, Error> {
        self.binding_check(binding)?;
        self.active_check(id)?;
        self.check(clock())?;
        if bytes.is_empty() || bytes.len() > MAX_CHUNK_BYTES {
            self.active.take();
            return Err(Error::Storage(receive::Error::InvalidChunk));
        }
        self.charge(RECORD_COST + bytes.len() as u64)?;
        let active = self.active.as_mut().expect("active checked");
        if let Err(error) = active.file.write_entry(index, offset, bytes) {
            self.active.take();
            return Err(Error::Storage(error));
        }
        if let Err(error) = self.check(clock()) {
            self.active.take();
            return Err(error);
        }
        Ok(self.progress().expect("active write"))
    }

    /// Flush/verify FIRST, then resample authority and permission immediately
    /// before rename. There is no policy lock across disk work. After rename,
    /// committed effects are always returned even if cancellation arrives while
    /// the directory is being synced. No automatic retry or receipt fabrication.
    pub fn complete(
        &mut self,
        binding: Binding,
        id: u64,
        mut clock: impl FnMut() -> HostInstant,
    ) -> Result<Receipt, Error> {
        self.binding_check(binding)?;
        self.active_check(id)?;
        self.check(clock())?;
        self.charge(RECORD_COST)?;
        let mut active = self.active.take().expect("active checked");
        active.file.verify().map_err(Error::Storage)?;
        let now = clock();
        self.check(now)?;
        if now >= active.deadline {
            self.closed = true;
            return Err(Error::Expired);
        }
        let publication = active.file.publish().map_err(Error::Storage)?;
        Ok(Receipt {
            id,
            bytes: active.size,
            publication,
        })
    }

    /// Service idle expiry as well as incoming records. The disk worker must
    /// call this on its timer turns even when a peer sends nothing. Failed
    /// cleanup is still charged by the storage reservation's fail-closed Drop.
    pub fn service(&mut self, now: HostInstant) -> Result<(), Error> {
        if let Err(error) = self.check(now) {
            self.active.take();
            return Err(error);
        }
        Ok(())
    }

    /// Cancellation stays available after expiry/permission loss and is never
    /// rate-limited. A stale cancel cannot remove a newer transfer's temporary.
    pub fn cancel(&mut self, binding: Binding, id: u64) -> Result<(), Error> {
        self.binding_check(binding)?;
        self.active_check(id)?;
        self.active
            .take()
            .expect("active checked")
            .file
            .cancel()
            .map_err(Error::Storage)
    }
    /// Fence before cleanup; ending files never revokes the desktop input lease.
    pub fn close(&mut self) -> Result<(), Error> {
        self.closed = true;
        self.active.take().map_or(Ok(()), |active| {
            active.file.cancel().map_err(Error::Storage)
        })
    }

    fn binding_check(&self, binding: Binding) -> Result<(), Error> {
        if binding == self.binding {
            Ok(())
        } else {
            Err(Error::WrongBinding)
        }
    }
    fn active_check(&self, id: u64) -> Result<(), Error> {
        if self.active.as_ref().is_some_and(|active| active.id == id) {
            Ok(())
        } else {
            Err(Error::NoTransfer)
        }
    }
    fn check(&mut self, now: HostInstant) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        let error = if now < self.last_time {
            Some(Error::Clock)
        } else if !self.permission.is_approved() {
            Some(Error::Permission)
        } else if let Err(error) = self.authority.deadline(now) {
            Some(Error::Authority(error))
        } else if self
            .active
            .as_ref()
            .is_some_and(|active| now >= active.deadline)
        {
            Some(Error::Expired)
        } else {
            None
        };
        if let Some(error) = error {
            self.closed = true;
            return Err(error);
        }
        let elapsed = now.as_micros() - self.last_time.as_micros();
        self.tokens = (self.tokens
            + u128::from(elapsed) * u128::from(self.policy.bytes_per_second))
        .min(u128::from(self.policy.burst_bytes) * MICRO);
        self.last_time = now;
        Ok(())
    }
    fn charge(&mut self, bytes: u64) -> Result<(), Error> {
        let cost = u128::from(bytes) * MICRO;
        if cost > self.tokens {
            return Err(Error::RateLimited);
        }
        self.tokens -= cost;
        Ok(())
    }
}
