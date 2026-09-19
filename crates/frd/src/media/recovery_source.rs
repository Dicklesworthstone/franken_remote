//! Split-borrow recovery admission into the ORIGINAL capture owner's single queue.
//! Only bounded policy work takes this mutex; no native call, await or callback
//! runs under it. Handles cannot keep a dropped source alive or survive unrestricted
//! worker access, and replacing provenance never replenishes the encoder rate.
use super::{CaptureSource, Error, ObservationControl, Subscription, host_now};
use fr_core::ids::CodecConfigurationGeneration;
use fr_media::delivery::{DeliveryError, IdrCoalescer, RecoveryDemand, SendCache};
use fr_wire::decoder::Binding;
use std::sync::{Arc, Mutex, Weak};

pub(super) struct SourceRecovery(Arc<Mutex<Queue>>);
struct Queue {
    source: Arc<()>,
    scheduler: IdrCoalescer,
}
impl SourceRecovery {
    pub(super) fn new(source: &Arc<()>) -> Result<Self, Error> {
        Ok(Self(Arc::new(Mutex::new(Queue {
            source: source.clone(),
            scheduler: IdrCoalescer::new(500_000).map_err(Error::Receiver)?,
        }))))
    }
    pub(super) fn queue(
        &mut self,
        cache: &SendCache,
        demand: RecoveryDemand,
        now: u64,
    ) -> Result<(), DeliveryError> {
        self.0
            .lock()
            .map_err(|_| DeliveryError::WrongState)?
            .scheduler
            .queue(cache, demand, now)
    }
    /// Charge only the original source scheduler. Newcomer expiry cannot abort
    /// a capture still needed by healthy viewers: it limits final join admission,
    /// not that capture's native timeout. A loss recovery retains its original
    /// cap; its already-admitted IDR also satisfies coincident waiting joins.
    pub(super) fn admit_capture(
        &mut self,
        now: u64,
        join_until: Option<u64>,
    ) -> Result<(bool, Option<u64>), DeliveryError> {
        let mut queue = self.0.lock().map_err(|_| DeliveryError::WrongState)?;
        let recovery_until = match queue.scheduler.take(now) {
            Ok(until) => until,
            Err(DeliveryError::RecoveryExpired) => None,
            Err(error) => return Err(error),
        };
        let join_idr = if recovery_until.is_none()
            && let Some(until) = join_until.filter(|&until| now < until)
        {
            queue.scheduler.take_for_join(now, until)?.is_some()
        } else {
            false
        };
        Ok((recovery_until.is_some() || join_idr, recovery_until))
    }
    pub(super) fn next_deadline(&self) -> Option<u64> {
        self.0.lock().ok()?.scheduler.next_deadline()
    }
    pub(super) fn retire(&mut self, source: &Arc<()>) {
        match self.0.lock() {
            Ok(mut queue) => {
                queue.source = source.clone();
                queue.scheduler.cancel_pending();
            }
            Err(poisoned) => poisoned.into_inner().scheduler.close(),
        }
    }
}

/// A non-authoritative handle to one actual capture source. A network dispatcher
/// may retain it while the capture loop exclusively borrows that source across
/// native IPC. It contains no worker, codec, unbounded queue, or input grant.
/// Every request still consumes the original subscription's admission policy.
pub struct CaptureRecovery {
    queue: Weak<Mutex<Queue>>,
    source: Arc<()>,
    configuration: CodecConfigurationGeneration,
    selected_control: Option<ObservationControl>,
}
impl std::fmt::Debug for CaptureRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CaptureRecovery([original source])")
    }
}
impl Subscription {
    /// Bind only after this subscription consumed an actual result from this
    /// worker. Matching numeric frame/config IDs are not source identity.
    /// The returned handle does not retain or borrow the worker itself.
    pub fn recovery_target(&self, source: &CaptureSource) -> Result<CaptureRecovery, Error> {
        self.control.check()?;
        let target = CaptureRecovery {
            queue: Arc::downgrade(&source.recovery.0),
            source: source.source.clone(),
            configuration: source.configuration.generation,
            selected_control: source.selected_control.clone(),
        };
        target.check(self, &source.source)?;
        Ok(target)
    }
}
impl CaptureRecovery {
    fn check(&self, subscription: &Subscription, current: &Arc<()>) -> Result<(), Error> {
        if !Arc::ptr_eq(&self.source, current)
            || subscription.epoch.configuration != self.configuration
            || subscription
                .capture_source
                .as_ref()
                .is_none_or(|id| !Arc::ptr_eq(id, &self.source))
            || self.selected_control.as_ref().is_some_and(|control| {
                !Arc::ptr_eq(&control.authority, &subscription.control.authority)
            })
        {
            return Err(Error::InvalidFrame);
        }
        Ok(())
    }

    /// Admit a bounded recovery record on an already authenticated control route.
    /// `binding` is the locally installed full view, never a peer-proposed tuple.
    /// This is the split-owner equivalent of `Subscription::request_recovery`:
    /// it fences old input/media before queueing work for the same native encoder.
    /// Duplicates cannot extend the original deadline or replenish rate credit.
    /// It does not install channels, resume a decoder, or silently reacquire input.
    pub fn request(
        &self,
        subscription: &mut Subscription,
        bytes: &[u8],
        binding: Binding,
    ) -> Result<bool, Error> {
        subscription.control.check()?;
        let shared = self.queue.upgrade().ok_or(Error::InvalidFrame)?;
        let mut queue = shared.lock().map_err(|_| Error::Poisoned)?;
        self.check(subscription, &queue.source)?;
        // Keep provenance validation and admission atomic with worker retirement.
        // Lock order is source -> authority, and neither lock crosses native work.
        let Some(demand) = subscription.admit_recovery_request(bytes, binding)? else {
            return Ok(false);
        };
        queue
            .scheduler
            .queue(
                &subscription.cache,
                demand,
                host_now(&subscription.control.cx)?.as_micros(),
            )
            .map_err(Error::Receiver)?;
        Ok(true)
    }
}
