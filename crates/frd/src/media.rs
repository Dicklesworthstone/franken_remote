//! Joins observation authority, process workers and the production media engine.
//! Admission remains Tailscale's job; this module accepts an ALREADY authorized
//! `SessionAuthority`. It creates neither identity nor an alternate transport.
use crate::worker::{self, Deadline, Launch, Worker};
use asupersync::{
    cx::Cx,
    types::{CancelKind, Time},
};
use fr_core::{
    authority::{AuthorityError, SessionAuthority},
    time::HostInstant,
};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    delivery::{
        BudgetUsage, DecodedFrame, DecoderBinding, DeliveryError, DeliveryMode, MediaBindings,
        MediaEpoch, PacketOffer, ReceivePipeline, SendCache, SendError, SendPolicy,
    },
    worker::{Configuration, Kind, Role},
};
use fr_wire::{FrameDescriptor, MediaLimits, PipelineState, Progress, SourceObservation};
use std::{
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};
mod capture_update;
pub mod clock;
pub mod decoder_startup;
#[cfg(target_os = "linux")]
pub mod discovery;
mod grant;
pub(crate) mod presentation;
pub(crate) mod receiver_feedback;
pub use receiver_feedback::Error as ReceiverFeedbackError;
pub mod renewal;
#[cfg(target_os = "linux")]
pub mod streaming;
pub use capture_update::CaptureUpdate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Authority(AuthorityError),
    Admission(fr_tailnet::Error),
    Worker(worker::Error),
    Send(SendError),
    InvalidFrame,
    Pacing(fr_media::pacing::Error),
    Backpressure,
    Delivery,
    Receiver(DeliveryError),
    Poisoned,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
impl From<worker::Error> for Error {
    fn from(e: worker::Error) -> Self {
        Self::Worker(e)
    }
}
impl From<SendError> for Error {
    fn from(e: SendError) -> Self {
        Self::Send(e)
    }
}
impl From<fr_media::worker::Error> for Error {
    fn from(e: fr_media::worker::Error) -> Self {
        Self::Worker(worker::Error::Protocol(e))
    }
}
pub fn host_now(cx: &Cx) -> Result<HostInstant, Error> {
    let clock = cx.timer_driver().ok_or(worker::Error::MissingRuntime)?;
    Ok(HostInstant::from_micros(clock.now().as_nanos() / 1000))
}
/// Clones share one authority, not copies of it. The dedicated session Cx must
/// descend from its runtime region; do not supply a daemon-wide Cx. Local revoke
/// closes authority FIRST, then cancels its in-flight operations without taking
/// a media-worker lock. No authority mutex is held across IPC or codec work.
#[derive(Clone)]
pub struct ObservationControl {
    authority: Arc<Mutex<SessionAuthority>>,
    cx: Cx,
    admission: Option<fr_tailnet::Lease>,
    renewal_attached: Arc<std::sync::atomic::AtomicBool>,
    control_grant_attached: Arc<std::sync::atomic::AtomicBool>,
}
impl ObservationControl {
    pub fn new(cx: Cx, mut authority: SessionAuthority) -> Result<Self, Error> {
        authority
            .authorize_observation_delivery(host_now(&cx)?)
            .map_err(Error::Authority)?;
        Ok(Self {
            authority: Arc::new(Mutex::new(authority)),
            cx,
            admission: None,
            renewal_attached: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            control_grant_attached: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
    /// Bind the already locally approved application authority to live Tailscale
    /// admission. Lookup success alone is not local consent or media readiness.
    /// The caller retains the non-cloneable admission owner and refreshes it.
    pub fn new_admitted(
        cx: Cx,
        authority: SessionAuthority,
        admission: fr_tailnet::Lease,
    ) -> Result<Self, Error> {
        admission.observe().map_err(Error::Admission)?;
        let mut control = Self::new(cx, authority)?;
        control.admission = Some(admission);
        control.check()?;
        Ok(control)
    }
    fn admission_deadline(&self) -> Result<Option<u64>, Error> {
        let Some(admission) = &self.admission else {
            return Ok(None);
        };
        match admission.observe() {
            Ok(until) => Ok(Some(until)),
            Err(error) => {
                self.revoke();
                Err(Error::Admission(error))
            }
        }
    }
    #[cfg(target_os = "linux")]
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.authority, &other.authority)
    }
    pub(crate) fn context(&self) -> Cx {
        self.cx.clone()
    }
    pub fn check(&self) -> Result<HostInstant, Error> {
        self.cx.checkpoint().map_err(|_| worker::Error::Cancelled)?;
        self.admission_deadline()?;
        let mut authority = self.authority.lock().map_err(|_| Error::Poisoned)?;
        let now = host_now(&self.cx)?;
        authority
            .authorize_observation_delivery(now)
            .map_err(Error::Authority)?;
        Ok(now)
    }
    /// Routing metadata must never borrow another approved session's authority.
    pub(crate) fn belongs_to_session(&self, session: fr_core::ids::RemoteSessionId) -> bool {
        self.authority.lock().is_ok_and(|a| a.session() == session)
    }
    /// Join an already locally granted native input lease to this exact approved
    /// observation owner. No copy, new grant, readiness claim or admission is
    /// created. The containing OS share-session must reserve its single Seat and
    /// use this owner's runtime clock for the native agent and network driver.
    pub fn input_session(
        &self,
        credentials: fr_core::input::InputCredentials,
        bounds: fr_core::input::InputBounds,
        capabilities: fr_core::input_submission::Capabilities,
    ) -> Result<fr_core::input_submission::InputSession, fr_core::input_submission::Refusal> {
        self.check()
            .map_err(|_| fr_core::input_submission::Refusal::Revoked)?;
        if let Some(admission) = &self.admission {
            admission
                .control()
                .map_err(|_| fr_core::input_submission::Refusal::Revoked)?;
        }
        fr_core::input_submission::InputSession::from_shared_authority(
            self.authority.clone(),
            credentials,
            bounds,
            capabilities,
            host_now(&self.cx).map_err(|_| fr_core::input_submission::Refusal::Revoked)?,
        )
    }
    pub(crate) fn check_control(&self) -> Result<HostInstant, Error> {
        self.check()?;
        if let Some(admission) = &self.admission {
            admission.control().map_err(Error::Admission)?;
        }
        let mut authority = self.authority.lock().map_err(|_| Error::Poisoned)?;
        let now = host_now(&self.cx)?;
        if !authority.has_live_control(now) {
            return Err(Error::Authority(AuthorityError::NoLease));
        }
        Ok(now)
    }
    pub(crate) fn owns_control_lease(
        &self,
        lease: &fr_core::input_submission::ControlLease,
    ) -> bool {
        lease.uses_authority(&self.authority)
    }
    pub fn deadline(&self, maximum: Duration) -> Result<Deadline, Error> {
        let peer_until = self.admission_deadline()?;
        let mut authority = self.authority.lock().map_err(|_| Error::Poisoned)?;
        let until = authority
            .observation_deadline(host_now(&self.cx)?)
            .map_err(Error::Authority)?;
        let until = peer_until.map_or(until.as_micros(), |peer| peer.min(until.as_micros()));
        let nanos = until.checked_mul(1000).ok_or(worker::Error::Deadline)?;
        Ok(Deadline::after(&self.cx, maximum)?.capped_at(Time::from_nanos(nanos)))
    }
    pub fn revoke(&self) {
        if let Some(admission) = &self.admission {
            admission.revoke();
        }
        if let Ok(mut a) = self.authority.lock() {
            a.close();
        }
        self.cx.cancel_fast(CancelKind::User);
    }
    pub fn suspend(&self) {
        if let Some(admission) = &self.admission {
            admission.revoke();
        }
        if let Ok(mut a) = self.authority.lock() {
            a.invalidate_for_suspend();
        }
        self.cx.cancel_fast(CancelKind::ParentCancelled);
    }
    pub fn issue_challenge(&self, nonce: u128) -> Result<HostInstant, Error> {
        self.check()?;
        self.authority
            .lock()
            .map_err(|_| Error::Poisoned)?
            .issue_observation_challenge(nonce, host_now(&self.cx)?)
            .map_err(Error::Authority)
    }
    pub fn renew(&self, nonce: u128) -> Result<HostInstant, Error> {
        self.check()?;
        self.authority
            .lock()
            .map_err(|_| Error::Poisoned)?
            .respond_observation_challenge(nonce, host_now(&self.cx)?)
            .map_err(Error::Authority)
    }
}
/// OS share-session-owned capture worker. The broker keeps this separate from
/// per-viewer Subscription objects. At most one capture is in flight; callers
/// cannot accumulate raw frames while a codec stalls. Sharing one encoder among
/// viewers additionally requires the broker's bounded fanout admission.
pub struct CaptureSource {
    worker: Worker,
    configuration: Configuration,
    next: Option<FrameId>,
    source: Arc<()>,
    last_capture: Option<FrameId>,
    selected_control: Option<ObservationControl>,
}
impl CaptureSource {
    pub async fn start(
        control: &ObservationControl,
        launch: Launch,
        configuration: Configuration,
    ) -> Result<Self, Error> {
        control.check()?;
        let worker = Worker::start(
            &control.cx,
            launch,
            configuration,
            control.deadline(Duration::from_secs(2))?,
        )
        .await?;
        if worker.role() != Role::Capture {
            return Err(Error::InvalidFrame);
        }
        control.check()?;
        Ok(Self {
            worker,
            configuration,
            next: Some(FrameId::FIRST),
            source: Arc::new(()),
            last_capture: None,
            selected_control: None,
        })
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.worker.id()
    }
    /// Retire provenance for future results before unrestricted worker access.
    /// This borrow can replace the child or its codec history. Resuming source-
    /// bound delivery requires a fresh encoded IDR and a new/recovered
    /// subscription; an unchanged reply cannot bridge this ownership boundary.
    pub fn worker_mut(&mut self) -> &mut Worker {
        if let Some(control) = &self.selected_control {
            control.revoke();
        }
        self.source = Arc::new(());
        self.last_capture = None;
        &mut self.worker
    }
}
/// A viewer's cache and generation do not own the shared capture worker. No
/// media packet is even encoded after its observation authority is revoked.
/// The transport MUST also call `authorize_write` immediately before each actual
/// enqueue/write, using the `PacketOffer`'s unchanged absolute send deadline.
pub struct Subscription {
    control: ObservationControl,
    cache: SendCache,
    limits: MediaLimits,
    epoch: MediaEpoch,
    first: bool,
    capture_source: Option<Arc<()>>,
}
impl Subscription {
    pub fn new(
        control: ObservationControl,
        limits: MediaLimits,
        bindings: MediaBindings,
        epoch: MediaEpoch,
        policy: SendPolicy,
    ) -> Result<Self, Error> {
        control.check()?;
        Ok(Self {
            control,
            cache: SendCache::new(limits, bindings, epoch, policy)?,
            limits,
            epoch,
            first: true,
            capture_source: None,
        })
    }
    pub(crate) fn maximum_capacity(&self) -> usize {
        self.cache.maximum_capacity()
    }
    pub(crate) fn stream_credit(&self, capacity: usize) -> bool {
        self.cache.can_push_capacity(capacity)
    }
    pub(crate) fn originals_pending(&self) -> bool {
        self.cache.originals_pending()
    }
    pub(crate) fn join_source(
        &self,
        source: &CaptureSource,
        control: &ObservationControl,
        epoch: MediaEpoch,
    ) -> Result<(), Error> {
        control.check()?;
        if !self.control.same_owner(control)
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
                .is_some_and(|c| !c.same_owner(control))
        {
            return Err(Error::InvalidFrame);
        }
        Ok(())
    }
    /// Exact record bound used by this subscription's packetizer.
    pub const fn record_bytes(&self) -> usize {
        self.limits.record_bytes()
    }
    pub fn enqueue(&mut self, unit: EncodedAccessUnit) -> Result<(), Error> {
        let now = self.control.check()?;
        if unit.config_generation() != self.epoch.configuration
            || unit.capture_micros() > now.as_micros()
            || (self.first && !unit.is_idr())
        {
            return Err(Error::InvalidFrame);
        }
        let reference = match unit.kind() {
            FrameKind::Idr { .. } => None,
            FrameKind::Predicted { references } => Some(references.as_raw()),
        };
        let progress = Progress {
            descriptor: FrameDescriptor {
                frame: unit.frame().as_raw(),
                reference,
                total_bytes: u32::try_from(unit.bytes().len()).map_err(|_| Error::InvalidFrame)?,
                stride: self.limits.fragment_stride(),
                capture_micros: unit.capture_micros(),
            },
            // A successful worker capture confirms service sometime after the
            // request instant. Report that conservative lower bound, not receipt time.
            observed_micros: unit.capture_micros(),
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        };
        self.cache.push(
            progress,
            unit.into_bytes(),
            if self.first {
                DeliveryMode::Recovery
            } else {
                DeliveryMode::Datagrams
            },
            now.as_micros(),
        )?;
        self.first = false;
        // Unproven lower-level frames cannot inherit another source's evidence.
        self.capture_source = None;
        Ok(())
    }
    pub fn next_packet(&mut self, out: &mut [u8]) -> Result<Option<PacketOffer>, Error> {
        let now = self.control.check()?;
        Ok(self.cache.next_packet(now.as_micros(), out)?)
    }
    pub fn authorize_write(&mut self, offer: &PacketOffer) -> Result<(), Error> {
        self.cache
            .authorize_write(offer, self.control.check()?.as_micros())?;
        Ok(())
    }
    /// Install an admitted recovery generation for this viewer only. The
    /// caller first fences its old input/view and abandons old transport sends,
    /// then installs matching receiver/channel bindings. The shared capture
    /// worker and healthy subscribers retain their own lifetimes.
    ///
    /// Only recovery within the same codec configuration is supported here;
    /// configuration changes require a separately configured worker. Retained
    /// repair spending is never replenished. The next enqueue must be an IDR
    /// and uses the dedicated reliable recovery channel.
    pub fn recover(&mut self, epoch: MediaEpoch, bindings: MediaBindings) -> Result<(), Error> {
        let now = self.control.check()?;
        if epoch.configuration != self.epoch.configuration {
            return Err(Error::InvalidFrame);
        }
        self.cache.replace(epoch, bindings, now.as_micros())?;
        self.epoch = epoch;
        self.first = true;
        self.capture_source = None;
        Ok(())
    }
    pub fn queue_repair(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.cache
            .queue_repair(bytes, self.control.check()?.as_micros())?;
        Ok(())
    }
    pub fn next_repair(&mut self, out: &mut [u8]) -> Result<Option<PacketOffer>, Error> {
        Ok(self
            .cache
            .next_repair_packet(self.control.check()?.as_micros(), out)?)
    }
    /// Charged encoded capacities and per-picture metadata. Fixed cache/repair
    /// storage is part of `size_of::<Subscription>()`, not this dynamic charge.
    pub const fn cache_usage(&self) -> BudgetUsage {
        BudgetUsage {
            bytes: self.cache.cached_bytes(),
            pictures: self.cache.cached_pictures(),
        }
    }
    /// Cache deadline only; authority expiry needs its independent watchdog.
    /// The owning Asupersync task must service this even when video goes idle.
    /// Reading the deadline does not renew authority or extend cache retention.
    pub fn next_deadline(&self) -> Option<HostInstant> {
        self.cache.next_deadline().map(HostInstant::from_micros)
    }
    /// Expire cache entries without requiring another capture, packet or repair.
    /// On authority/cancellation failure the owner must retire this subscription;
    /// dropping it releases its cache. This method never extends authority.
    pub fn tick(&mut self) -> Result<(), Error> {
        self.cache.tick(self.control.check()?.as_micros())?;
        Ok(())
    }
}
/// Viewer-local process owner. Compressed receiver reservations remain alive
/// until a real decoder/presenter completion; no success is inferred from IPC
/// submission. This local trusted-stream lane does not qualify hostile HEVC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationStage {
    DecodedOnly,
    SubmittedToCompositor,
}
#[derive(Debug)]
pub struct PresentationReceipt {
    pub frame: FrameId,
    pub stage: PresentationStage,
    /// Bound successful-decode completion. Consume this in the view tracker;
    /// neither this receipt nor compositor submission certifies visibility.
    pub decoded: DecodedFrame,
}
pub struct Presenter {
    worker: Worker,
    configuration: Configuration,
    binding: DecoderBinding,
    stream_binding: Option<(
        fr_transport::quic::ConnectionBinding,
        fr_wire::decoder::Binding,
    )>,
}
impl Presenter {
    pub async fn start(
        cx: &Cx,
        launch: Launch,
        configuration: Configuration,
        record: &fr_media::hevc::DecoderRecord,
        receiver: &mut ReceivePipeline,
    ) -> Result<Self, Error> {
        let limits = configuration.limits()?;
        receiver
            .check_decoder_configuration(configuration.generation, &limits)
            .map_err(Error::Receiver)?;
        let mut receiving = ReceiveOperation::new(receiver);
        let worker = Worker::start_decoder(
            cx,
            launch,
            configuration,
            record,
            Deadline::after(cx, Duration::from_secs(2))?,
        )
        .await?;
        if worker.role() != Role::Present {
            return Err(Error::InvalidFrame);
        }
        let binding = receiving
            .receiver
            .bind_decoder(configuration.generation, &limits, host_now(cx)?.as_micros())
            .map_err(Error::Receiver)?;
        receiving.completed = true;
        Ok(Self {
            worker,
            configuration,
            binding,
            stream_binding: None,
        })
    }
    /// Reuse this running decoder for a new IDR under the SAME receiver and
    /// codec configuration. The caller first fences input and old transport
    /// sends. A failed chain may recover; a foreign/replaced/closed receiver or
    /// poisoned worker cannot acquire a fabricated configured acknowledgement.
    pub fn recover(
        &mut self,
        cx: &Cx,
        receiver: &mut ReceivePipeline,
        epoch: MediaEpoch,
        bindings: MediaBindings,
    ) -> Result<(), Error> {
        self.binding
            .check_recovery(receiver)
            .map_err(Error::Receiver)?;
        if self.worker.state() != worker::State::Running {
            return Err(worker::Error::Unavailable.into());
        }
        if epoch.configuration != self.configuration.generation {
            return Err(Error::InvalidFrame);
        }
        cx.checkpoint().map_err(|_| worker::Error::Cancelled)?;
        let now = host_now(cx)?.as_micros();
        receiver
            .replace(epoch, bindings, now)
            .map_err(Error::Receiver)?;
        let mut receiving = ReceiveOperation::new(receiver);
        self.stream_binding = None;
        self.binding = receiving
            .receiver
            .bind_decoder(
                self.configuration.generation,
                &self.configuration.limits()?,
                now,
            )
            .map_err(Error::Receiver)?;
        receiving.completed = true;
        Ok(())
    }
    pub async fn present_next(
        &mut self,
        cx: &Cx,
        receiver: &mut ReceivePipeline,
    ) -> Result<Option<PresentationReceipt>, Error> {
        let Some(job) = self.take_next(cx, receiver)? else {
            return Ok(None);
        };
        // This compatibility call retains the receiver borrow across IPC, so
        // preserve its eager cancellation cleanup. The detached streaming path
        // uses DecodeJob's scope fence and services the receiver independently.
        let mut receiving = ReceiveOperation::new(receiver);
        let receipt = self
            .decode_job(cx, job)
            .await?
            .complete(cx, receiving.receiver)?;
        receiving.completed = true;
        Ok(Some(receipt))
    }
    /// Fence view receipts before stopping native work. No raw worker access is
    /// exposed: decoder submissions always pass through the bound receiver.
    pub async fn stop(&mut self, cx: &Cx, deadline: Deadline) -> Result<(), Error> {
        self.binding.revoke();
        let mut operation = MediaOperation::new(&mut self.worker);
        operation
            .worker
            .request(cx, Kind::Stop, vec![], deadline)
            .await?;
        operation.completed = true;
        Ok(())
    }
    pub async fn reap(
        &mut self,
        cx: &Cx,
        deadline: Deadline,
    ) -> Result<asupersync::process::ExitStatus, Error> {
        self.binding.revoke();
        Ok(self.worker.reap(cx, deadline).await?)
    }
    pub fn abort(&mut self) {
        self.binding.revoke();
        self.worker.abort();
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.worker.id()
    }
}
impl Drop for Presenter {
    fn drop(&mut self) {
        self.binding.revoke();
        self.worker.abort();
    }
}
/// Until a complete startup/decode transition, dropping its future invalidates
/// the receiving scope and releases queued reservations. External picture
/// owners keep their own charged bytes until they release them.
struct ReceiveOperation<'a> {
    receiver: &'a mut ReceivePipeline,
    completed: bool,
}
impl<'a> ReceiveOperation<'a> {
    fn new(receiver: &'a mut ReceivePipeline) -> Self {
        Self {
            receiver,
            completed: false,
        }
    }
}
impl Drop for ReceiveOperation<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.receiver.close();
        }
    }
}
/// Keep cancellation terminal across the WHOLE codec operation, including
/// cooperative waits BETWEEN IPC exchanges. A packet-level guard alone cannot
/// protect a decoder that already accepted input before returning `NeedInput`.
struct MediaOperation<'a> {
    worker: &'a mut Worker,
    completed: bool,
}
impl<'a> MediaOperation<'a> {
    fn new(worker: &'a mut Worker) -> Self {
        Self {
            worker,
            completed: false,
        }
    }
}
impl Drop for MediaOperation<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.worker.abort();
        }
    }
}
