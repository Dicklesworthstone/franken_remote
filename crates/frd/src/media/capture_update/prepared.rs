//! Reserve the actual parent-side IPC output capacity before issuing capture.
use super::{CaptureSource, Error, ObservationControl, SharedCaptureUpdate, Subscription};
use crate::{media_egress::Egress, worker};
use fr_media::{
    delivery::{SharedFramePool, SharedFrameReservation},
    worker::UNIT_PREFIX_BYTES,
};
use std::sync::Arc;

/// Exactly one next native capture on the original source. Network/renewal
/// service may keep using its separate egresses while this operation is pending.
/// Dropping before polling releases credit without issuing IPC; dropping during
/// IPC retains the existing capture operation's poison/abort behavior. The child
/// process has its separate native bounds; this reserves the parent output only.
#[must_use = "capture or drop the prepared operation to return its reservation"]
pub struct PreparedSharedCapture<'a> {
    source: &'a mut CaptureSource,
    control: &'a ObservationControl,
    reservation: SharedFrameReservation,
}
impl std::fmt::Debug for PreparedSharedCapture<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedSharedCapture")
            .field("maximum_capacity", &self.reservation.maximum_capacity())
            .finish_non_exhaustive()
    }
}
impl CaptureSource {
    /// Reserve the configured maximum output, including the IPC prefix retained
    /// by `parse_unit`, BEFORE frame IDs advance or a native request is issued.
    /// Backpressure is retryable without touching this source. An impossible
    /// pool/profile combination refuses rather than encoding unusable references.
    pub fn prepare_shared_capture<'a>(
        &'a mut self,
        control: &'a ObservationControl,
        pool: &SharedFramePool,
    ) -> Result<PreparedSharedCapture<'a>, Error> {
        control.check()?;
        if self
            .selected_control
            .as_ref()
            .is_some_and(|original| !Arc::ptr_eq(&original.authority, &control.authority))
        {
            return Err(Error::InvalidFrame);
        }
        if self.worker.state() != worker::State::Running {
            return Err(worker::Error::Unavailable.into());
        }
        let configuration = self.configuration.codec()?;
        let geometry = configuration.geometry();
        pool.limits()
            .validate_coded_dimensions(geometry.coded_width(), geometry.coded_height())
            .map_err(|_| Error::InvalidFrame)?;
        let maximum = usize::try_from(self.configuration.max_access_unit_bytes)
            .ok()
            .and_then(|n| n.checked_add(UNIT_PREFIX_BYTES))
            .ok_or(Error::InvalidFrame)?;
        if self.configuration.max_access_unit_bytes > pool.limits().max_encoded_access_unit_bytes()
            || maximum > pool.maximum_capacity()
        {
            return Err(Error::InvalidFrame);
        }
        let reservation = pool
            .reserve_capacity(maximum)
            .map_err(|error| match error {
                fr_media::delivery::DeliveryError::ResourceLimit => Error::Backpressure,
                other => Error::Receiver(other),
            })?;
        Ok(PreparedSharedCapture {
            source: self,
            control,
            reservation,
        })
    }
}
impl PreparedSharedCapture<'_> {
    /// Conservative per-viewer preflight using the FULL shared charge, not just
    /// encoded length or the legacy Vec charge. It does not retain a borrow on
    /// the recipient across native work. The source's serialized producer must
    /// not enqueue other outputs in between; final admission remains mandatory.
    /// A blocked recipient returns false without forcing other viewers to wait.
    /// First-viewer admission/bootstrap remains a separate decoder handshake.
    pub fn check_recipient(&self, recipient: &mut Egress) -> Result<bool, Error> {
        recipient.shared_capture_credit(self.source, self.reservation.charged_bytes())
    }
    pub fn reserved_bytes(&self) -> usize {
        self.reservation.charged_bytes()
    }
    /// Uses the existing source's conditional capture, recovery scheduler,
    /// selected-display checks, authority and fixed native deadline. No parallel
    /// encoder, clock, reference identity, or recovery rate allowance is created.
    pub async fn capture_if_changed(self, force_idr: bool) -> Result<SharedCaptureUpdate, Error> {
        self.capture(force_idr, None).await
    }
    /// Attempt one already-authorized late-join cohort on this source's original
    /// IDR/recovery rate allowance. Rate pressure yields normal dependent/static
    /// output, not a reset or a new encoder. Only an actual returned IDR can
    /// bootstrap a join. Expired joins do not poison healthy capture. Capacity
    /// remains reserved BEFORE the rate allowance is consumed or IPC is issued.
    /// The join owner must reject expired joins after completion; its deadline
    /// cannot shorten a capture still needed by healthy viewers. Native work
    /// retains its original fixed source/recovery deadline, never a renewed one.
    pub async fn capture_for_join(self, until_micros: u64) -> Result<SharedCaptureUpdate, Error> {
        self.capture(false, Some(until_micros)).await
    }
    async fn capture(
        self,
        force_idr: bool,
        join_until: Option<u64>,
    ) -> Result<SharedCaptureUpdate, Error> {
        let update = self
            .source
            .capture_request(
                self.control,
                force_idr,
                true,
                Some(self.reservation.maximum_capacity()),
                join_until,
            )
            .await?;
        let result = update.share_reserved(self.reservation);
        if result.is_err() {
            // The source already advanced. An impossible native output may not
            // silently disappear while subsequent dependent frames continue.
            self.source.worker.abort();
        }
        result
    }
}
impl Subscription {
    pub(crate) fn shared_capture_credit(
        &mut self,
        source: &CaptureSource,
        charged: usize,
    ) -> Result<bool, Error> {
        if self.first
            || source.configuration.generation != self.epoch.configuration
            || self
                .capture_source
                .as_ref()
                .is_none_or(|id| !Arc::ptr_eq(id, &source.source))
        {
            return Err(Error::InvalidFrame);
        }
        let result = self
            .control
            .check()
            .and_then(|now| self.cache.tick(now.as_micros()).map_err(Error::Send));
        if let Err(error) = result {
            if let Ok(mut authority) = self.control.authority.lock() {
                authority.mark_view_stale();
            }
            self.cache.close();
            return Err(error);
        }
        Ok(!self.originals_pending() && self.cache.can_push_capacity(charged))
    }
}
