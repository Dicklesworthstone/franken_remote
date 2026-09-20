//! One OS-share-session source, bounded independently revocable subscribers.
//! Transports and renewal remain with their original connection tasks. Handles
//! perform only bounded synchronous work; no policy mutex crosses native IPC.
use super::{
    CaptureSource, Error as MediaError, ObservationControl, SharedCaptureUpdate, Subscription,
    decoder_startup,
};
use crate::{
    media_egress::Lane,
    media_quic::{self, NegotiatedMedia, QuicEgress},
    worker,
};
use asupersync::{cx::Cx, process::ExitStatus};
use fr_media::delivery::{BudgetUsage, MediaEpoch, SharedFramePool};
use fr_transport::quic::{ConnectionBinding, QuicRecords, Route};
use fr_wire::decoder::Binding;
use std::{
    future::{Future, poll_fn},
    pin::pin,
    sync::{Arc, Mutex, Weak},
    task::Poll,
};

pub const MAX_SUBSCRIBERS: usize = 8;
const BOOTSTRAP_US: u64 = 2_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Media(MediaError),
    Transport(media_quic::Error),
    Startup(decoder_startup::Error),
    Closed,
    Full,
    NoSubscribers,
    SlowSubscriber,
    WrongSource,
    ForeignConnection,
    InvalidBudget,
    Poisoned,
    JoinExpired,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

struct Entry {
    connection: ConnectionBinding,
    media: NegotiatedMedia,
    control: ObservationControl,
    sender: QuicEgress,
    failure: Option<Error>,
    join: Option<join::PendingJoin>,
    starting: Option<pending::Starting>,
}
impl Entry {
    fn close(&mut self, error: Error) {
        // Fence the exact subscriber BEFORE dropping its retained media. This
        // never revokes the OS source or another remote session's authority.
        self.control.revoke();
        if let Some(mut starting) = self.starting.take() {
            starting.host.close();
        }
        self.sender.close();
        self.join = None;
        self.failure.get_or_insert(error);
    }
}
struct Members {
    entries: [Option<Entry>; MAX_SUBSCRIBERS],
    owner: ObservationControl,
    anchor: Option<Binding>,
    configuration: fr_media::worker::Configuration,
    started: bool,
    closed: bool,
    until: u64,
    last: u64,
}
impl Members {
    fn active(&self) -> usize {
        self.entries
            .iter()
            .flatten()
            .filter(|e| e.failure.is_none() && e.join.is_none())
            .count()
    }
    fn stop_if_empty(&mut self) {
        if self.started && self.active() == 0 {
            // Queued late joins never keep a source alive after its original
            // cohort leaves. Initial admit_pending members retain their existing
            // startup lifetime. Fence late joins and release bootstrap aliases;
            // the original child still requires Publisher::reap.
            self.close(Error::Closed);
        }
    }
    fn close(&mut self, error: Error) {
        for entry in self.entries.iter_mut().flatten() {
            entry.close(error);
        }
        self.closed = true;
        self.owner.revoke();
    }
    fn tick(&mut self) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        let now = match self.owner.check() {
            Ok(now) => now.as_micros(),
            Err(error) => {
                self.close(Error::Media(error));
                return Err(Error::Media(error));
            }
        };
        if now < self.last || (!self.started && now >= self.until) {
            self.close(Error::Closed);
            return Err(Error::Closed);
        }
        self.last = now;
        for entry in self
            .entries
            .iter_mut()
            .flatten()
            .filter(|e| e.failure.is_none())
        {
            if let Err(error) = entry.tick(now) {
                entry.close(error);
            }
        }
        self.stop_if_empty();
        if self.closed {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }
}

/// A bounded OS-share-session publication lifetime. `owner` must be the source's
/// independent observation authority, never the first viewer's authority. The
/// actual initial shared capture establishes source AND physical-pool identity.
/// No listener, permission, controller, encoder or new runtime is created here.
///
/// Admission consumes a completed handshake or retains an unstarted shared
/// handshake in its original bounded slot; late joins wait for a fresh IDR.
/// Network tasks retain their original HostSession/QuicRecords and use their
/// non-cloneable Subscriber during capture awaits. They must keep servicing
/// parent renewal/cancellation and close that parent on a subscriber error.
pub struct Publisher {
    source: CaptureSource,
    pool: SharedFramePool,
    members: Arc<Mutex<Members>>,
}
impl std::fmt::Debug for Publisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedPublisher")
            .field("physical_usage", &self.pool.usage())
            .finish_non_exhaustive()
    }
}
impl Publisher {
    pub fn new(
        source: CaptureSource,
        owner: ObservationControl,
        pool: SharedFramePool,
        bootstrap: &SharedCaptureUpdate,
    ) -> Result<Self, Error> {
        let now = owner.check().map_err(Error::Media)?.as_micros();
        bootstrap
            .check_publisher_source(&source, &pool)
            .map_err(Error::Media)?;
        if source
            .selected_control
            .as_ref()
            .is_some_and(|c| !c.same_owner(&owner))
        {
            return Err(Error::WrongSource);
        }
        let until = now.checked_add(BOOTSTRAP_US).ok_or(Error::Closed)?;
        let configuration = source.configuration;
        Ok(Self {
            source,
            pool,
            members: Arc::new(Mutex::new(Members {
                entries: core::array::from_fn(|_| None),
                owner,
                anchor: None,
                configuration,
                started: false,
                closed: false,
                until,
                last: now,
            })),
        })
    }
    /// The parent has already authorized this display and completed its native
    /// decoder handshake. Refuse copied routes, another source, shared authority,
    /// different display scope, duplicate sessions and lagging bootstrap frames.
    /// A late join needs a separately rate-admitted IDR; no implicit force occurs.
    pub fn admit(
        &mut self,
        startup: decoder_startup::Host,
        sender: QuicEgress,
        media: NegotiatedMedia,
        transport: &QuicRecords,
    ) -> Result<Subscriber, Error> {
        let mut members = self.members.lock().map_err(|_| Error::Poisoned)?;
        members.tick()?;
        let slot = members
            .entries
            .iter()
            .position(Option::is_none)
            .ok_or(Error::Full)?;
        let (control, view) = startup.finish_stream(transport).map_err(Error::Startup)?;
        if control.same_owner(&members.owner)
            || members.owner.belongs_to_session(view.parent.remote_session)
            || same_task(&control, &members.owner)
            || members.entries.iter().flatten().any(|e| {
                e.control.same_owner(&control)
                    || same_task(&e.control, &control)
                    || e.media.binding().parent.remote_session == view.parent.remote_session
            })
            || members.anchor.is_some_and(|a| !same_source_view(a, view))
        {
            return Err(Error::WrongSource);
        }
        sender
            .join_shared_publisher(
                transport,
                &media,
                &self.source,
                &members.owner,
                &control,
                view,
            )
            .map_err(Error::Transport)?;
        members.anchor.get_or_insert(view);
        members.started = true;
        members.entries[slot] = Some(Entry {
            connection: transport.binding(),
            media,
            control,
            sender,
            failure: None,
            join: None,
            starting: None,
        });
        Ok(Subscriber {
            members: Arc::downgrade(&self.members),
            slot,
        })
    }
    /// Service expiry even with a static display. Closed subscriber slots remain
    /// bounded and reserved until their original handles drop; there is no ID reuse.
    pub fn tick(&mut self) -> Result<usize, Error> {
        let result = self
            .members
            .lock()
            .map_err(|_| Error::Poisoned)
            .and_then(|mut m| {
                m.tick()?;
                Ok(m.active())
            });
        if result.is_err() {
            self.close();
        }
        result
    }
    pub fn physical_usage(&self) -> BudgetUsage {
        self.pool.usage()
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.source.worker_id()
    }

    /// One pre-reserved native observation for the current bounded cohort.
    /// Service all Subscriber handles on their original network tasks while this
    /// future awaits. No egress borrow or mutex is retained across native work.
    ///
    /// If every recipient lacks credit, no reference is produced. When another
    /// recipient can proceed, lagging recipients are explicitly refused BEFORE
    /// capture, not silently skipped or allowed to stall healthy viewers. Final
    /// per-viewer admission still checks revocation after native completion.
    /// Dropping even an unpolled operation terminates this publication lifetime.
    pub fn capture_next(&mut self) -> impl Future<Output = Result<CaptureReport, Error>> + '_ {
        let operation = PublicationOperation {
            publisher: self,
            complete: false,
        };
        async move {
            let mut operation = operation;
            let result = operation.publisher.capture_inner().await;
            // A successful capture can also retire its last recipient. End
            // native ownership immediately, not on an unrelated later packet.
            let closed = operation
                .publisher
                .members
                .lock()
                .map_or(true, |m| m.closed);
            if closed {
                operation.publisher.close();
            }
            if result.is_ok()
                || matches!(
                    result,
                    Err(Error::Media(MediaError::Backpressure) | Error::NoSubscribers)
                )
            {
                operation.complete = true;
            }
            result
        }
    }
    async fn capture_inner(&mut self) -> Result<CaptureReport, Error> {
        let owner = {
            let mut members = self.members.lock().map_err(|_| Error::Poisoned)?;
            members.tick()?;
            if members.active() == 0 {
                return Err(Error::NoSubscribers);
            }
            members.owner.clone()
        };
        let prepared = self
            .source
            .prepare_shared_capture(&owner, &self.pool)
            .map_err(Error::Media)?;
        let (mut refused, join_until) = self
            .members
            .lock()
            .map_err(|_| Error::Poisoned)?
            .capture_credit(&prepared)?;
        // Declare the fencing guard AFTER the native future: Rust drops it
        // first on cancellation, before the existing IPC future aborts the child.
        let mut capture = pin!(async move {
            match join_until {
                Some(until) => prepared.capture_for_join(until).await,
                None => prepared.capture_if_changed(false).await,
            }
        });
        let mut fence = CaptureGuard {
            members: self.members.clone(),
            complete: false,
        };
        // Borrow the pinned native future rather than moving it into await. The
        // later-declared guard therefore fences members before native Drop.
        let update = poll_fn(|task| {
            let live = self
                .members
                .lock()
                .map_err(|_| Error::Poisoned)
                .and_then(|mut m| m.tick());
            if let Err(error) = live {
                return Poll::Ready(Err(error));
            }
            capture
                .as_mut()
                .poll(task)
                .map(|result| result.map_err(Error::Media))
        })
        .await?;
        let mut members = self.members.lock().map_err(|_| Error::Poisoned)?;
        members.tick()?;
        let mut delivered = 0;
        for entry in members
            .entries
            .iter_mut()
            .flatten()
            .filter(|e| e.failure.is_none())
        {
            match entry.publish(&update) {
                Ok(true) => delivered += 1,
                Ok(false) => {}
                Err(error) => {
                    entry.close(error);
                    refused += 1;
                }
            }
        }
        members.stop_if_empty();
        let result = CaptureReport {
            frame: update.frame().as_raw(),
            unchanged: update.is_unchanged(),
            delivered,
            refused,
        };
        drop(update);
        fence.complete = true;
        Ok(result)
    }
    pub fn close(&mut self) {
        self.members
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .close(Error::Closed);
        self.source.worker.abort();
    }
    /// The original child remains collectable after last-subscriber departure,
    /// cancellation or source failure. Released parent bytes do not prove exit.
    pub async fn reap(
        &mut self,
        cleanup: &Cx,
        deadline: worker::Deadline,
    ) -> Result<ExitStatus, worker::Error> {
        self.close();
        self.source.worker.reap(cleanup, deadline).await
    }
}
impl Drop for Publisher {
    fn drop(&mut self) {
        self.close();
    }
}
struct PublicationOperation<'a> {
    publisher: &'a mut Publisher,
    complete: bool,
}
impl Drop for PublicationOperation<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.publisher.close();
        }
    }
}
struct CaptureGuard {
    members: Arc<Mutex<Members>>,
    complete: bool,
}
impl Drop for CaptureGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.members
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .close(Error::Closed);
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureReport {
    pub frame: u64,
    pub unchanged: bool,
    pub delivered: usize,
    pub refused: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendReport {
    pub accepted: usize,
    pub pending: bool,
}

/// Unique membership in one source's bounded cohort, NOT a connection or input
/// grant. Cannot retain a dropped publisher, migrate to equal numeric routes,
/// or be cloned to duplicate sends. Drop removes only this viewer's state.
pub struct Subscriber {
    members: Weak<Mutex<Members>>,
    slot: usize,
}
impl std::fmt::Debug for Subscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedSubscriber([original owner])")
    }
}
impl Subscriber {
    fn with_entry<T>(
        &mut self,
        transport: &QuicRecords,
        use_entry: impl FnOnce(&mut Entry) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let shared = self.members.upgrade().ok_or(Error::Closed)?;
        let mut members = shared.lock().map_err(|_| Error::Poisoned)?;
        let entry = members.entries[self.slot].as_ref().ok_or(Error::Closed)?;
        if !transport.is_bound_to(&entry.connection) {
            return Err(Error::ForeignConnection);
        }
        if let Some(error) = entry.failure {
            return Err(error);
        }
        members.tick()?;
        let entry = members.entries[self.slot].as_mut().ok_or(Error::Closed)?;
        if let Some(error) = entry.failure {
            return Err(error);
        }
        let result = entry
            .media
            .check(transport)
            .map_err(Error::Transport)
            .and_then(|()| use_entry(entry));
        if let Err(error) = result {
            entry.close(error);
        }
        members.stop_if_empty();
        result
    }
    /// Bounded admission of actual packets on the original transport. A pending
    /// packet is retained unchanged, and another connection can run immediately.
    /// Configuration counts against the same send budget as media. This does
    /// not drive UDP, renew observation, execute a decoder or grant input.
    pub fn service(
        &mut self,
        cx: &Cx,
        transport: &mut QuicRecords,
        maximum_records: usize,
    ) -> Result<SendReport, Error> {
        if !(1..=64).contains(&maximum_records) {
            return Err(Error::InvalidBudget);
        }
        // Identity preflight borrows transport only momentarily; no caller
        // callback executes inside the group mutex.
        let shared = self.members.upgrade().ok_or(Error::Closed)?;
        let mut members = shared.lock().map_err(|_| Error::Poisoned)?;
        let entry = members.entries[self.slot].as_ref().ok_or(Error::Closed)?;
        if !transport.is_bound_to(&entry.connection) {
            return Err(Error::ForeignConnection);
        }
        if let Some(error) = entry.failure {
            return Err(error);
        }
        members.tick()?;
        let owner = members.owner.clone();
        let entry = members.entries[self.slot].as_mut().ok_or(Error::Closed)?;
        if let Some(error) = entry.failure {
            return Err(error);
        }
        let result = (|| {
            entry.media.check(transport).map_err(Error::Transport)?;
            let mut report = SendReport {
                accepted: 0,
                pending: false,
            };
            entry.service_startup(transport, &owner, &mut report)?;
            if !entry.service_join(transport, &owner, &mut report)? {
                report.pending = true;
                return Ok(report);
            }
            for _ in report.accepted..maximum_records {
                if entry.starting.as_ref().is_some_and(|s| !s.seeded) {
                    break;
                }
                let original = entry
                    .sender
                    .transmit_startup_authorized(
                        cx,
                        transport,
                        entry.starting.as_ref().map(|s| s.first),
                        || owner.check().is_ok(),
                    )
                    .map_err(Error::Transport)?;
                let progress = if original == crate::media_egress::Progress::Idle
                    && entry.starting.is_none()
                {
                    entry
                        .sender
                        .transmit_authorized(cx, transport, Lane::Repair, || owner.check().is_ok())
                        .map_err(Error::Transport)?
                } else {
                    original
                };
                match progress {
                    crate::media_egress::Progress::Accepted(_) => report.accepted += 1,
                    crate::media_egress::Progress::Pending(_) => {
                        report.pending = true;
                        break;
                    }
                    crate::media_egress::Progress::Idle => break,
                }
            }
            Ok(report)
        })();
        if let Err(error) = result {
            entry.close(error);
        }
        if let Err(error) = owner.check() {
            members.close(Error::Media(error));
        }
        members.stop_if_empty();
        result
    }
    /// Consume only this subscriber's actual repair route. Other control records
    /// remain with the original session, including renewal and recovery policy.
    pub fn repair(
        &mut self,
        transport: &QuicRecords,
        route: Route,
        bytes: &[u8],
    ) -> Result<Option<media_quic::RepairAdmission>, Error> {
        self.with_entry(transport, |entry| {
            if route != entry.sender.stream_repair_route() {
                return Ok(None);
            }
            if entry.starting.is_some() || entry.join.is_some() {
                return Err(Error::Startup(decoder_startup::Error::WrongState));
            }
            entry
                .sender
                .repair_on(transport, route, bytes)
                .map(Some)
                .map_err(Error::Transport)
        })
    }
}
impl Drop for Subscriber {
    fn drop(&mut self) {
        if let Some(shared) = self.members.upgrade() {
            let mut members = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(mut entry) = members.entries[self.slot].take() {
                entry.close(Error::Closed);
            }
            members.stop_if_empty();
        }
    }
}
// Equal runtime task ownership is a definite cancellation-sharing mistake,
// even when callers constructed different numeric SessionAuthority values.
// Callers must still provide independent sibling session contexts in one runtime.
fn same_task(a: &ObservationControl, b: &ObservationControl) -> bool {
    a.cx.task_id() == b.cx.task_id() && a.cx.region_id() == b.cx.region_id()
}
fn same_source_view(a: Binding, b: Binding) -> bool {
    (
        a.parent.host_boot,
        a.parent.os_session,
        a.display,
        a.geometry,
        a.configuration,
    ) == (
        b.parent.host_boot,
        b.parent.os_session,
        b.display,
        b.geometry,
        b.configuration,
    )
}
impl Subscription {
    pub(crate) fn join_shared_source(
        &self,
        source: &CaptureSource,
        owner: &ObservationControl,
        control: &ObservationControl,
        epoch: MediaEpoch,
    ) -> Result<(), MediaError> {
        owner.check()?;
        control.check()?;
        if !self.control.same_owner(control)
            || owner.same_owner(control)
            || self.epoch != epoch
            || self.first
            || source.configuration.generation != epoch.configuration
            || self
                .capture_source
                .as_ref()
                .is_none_or(|id| !Arc::ptr_eq(id, &source.source))
            || source
                .selected_control
                .as_ref()
                .is_some_and(|c| !c.same_owner(owner))
            || self.source_progress().map(|p| p.descriptor.frame)
                != source
                    .last_capture
                    .map(fr_media::access_unit::FrameId::as_raw)
        {
            return Err(MediaError::InvalidFrame);
        }
        Ok(())
    }
}

mod join;
mod pending;
mod service;
mod session;
pub use join::JoinQueue;

pub(crate) mod consent;
