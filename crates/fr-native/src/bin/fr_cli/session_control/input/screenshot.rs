//! Pixel reads pin the SAME original input observation, never a replacement
//! status sample after export. The local socket never submits an input action.
use super::{Candidate, Shared};
use crate::session_control::screenshot::{Metadata, Refusal};
use fr_client::robot::{
    live::{Evidence, Observation},
    snapshot::{Handle, Image},
};
use fr_media::{access_unit::FrameId, worker::presentation_snapshot::Stamp};
use fr_wire::display::Display;
use std::time::Instant;

impl Shared {
    pub fn bind_snapshot(
        &self,
        handle: Handle,
        display: Display,
        frame: u64,
        evidence: Evidence,
    ) -> Result<(), ()> {
        let mut observation = Observation::default();
        observation.begin(1).map_err(|_| ())?;
        observation.authenticated().map_err(|_| ())?;
        observation
            .ready(display, frame, evidence, Instant::now())
            .map_err(|_| ())?;
        let mut state = self.locked();
        if state.stopped || state.snapshots.is_some() {
            return Err(());
        }
        state.observation = observation;
        state.snapshots = Some(handle);
        Ok(())
    }
    pub fn presented(&self, frame: u64, evidence: Evidence) -> Result<(), ()> {
        self.locked()
            .observation
            .presented(frame, evidence, Instant::now())
            .map_err(|_| ())
    }
    pub(crate) async fn capture(&self) -> Result<(Metadata, Image), Refusal> {
        // Only scalar metadata and the cheap request handle are touched under
        // the state lock. The actual worker request is driven by StreamingViewer.
        let (request, mut observation, mut control, original, began) = {
            let mut state = self.locked();
            if state.pending.is_some() {
                return Err(Refusal::Busy);
            }
            let handle = state.snapshots.clone().ok_or(Refusal::Unavailable)?;
            let control = state.status();
            if !control.ready {
                return Err(Refusal::NotReady);
            }
            let (_, candidate) = state.pinned.ok_or(Refusal::NotReady)?;
            let began = Instant::now();
            let observation = state
                .observation
                .snapshot(began)
                .map_err(|_| Refusal::NotCurrent)?;
            let frame = observation.frame.as_ref().ok_or(Refusal::NotReady)?;
            if frame.frame_serial != candidate.sample.stamp.frame
                || frame.geometry_generation != candidate.view.geometry.as_raw()
                || frame.evidence_level != Evidence::SubmittedToCompositor
            {
                return Err(Refusal::NotCurrent);
            }
            let stamp = Stamp {
                frame: FrameId::from_raw(candidate.sample.stamp.frame),
                configuration: candidate.view.configuration,
                recovery: candidate.view.recovery,
            };
            let request = handle.request(stamp).map_err(Refusal::from)?;
            (request, observation, control, candidate, began)
        };
        let image = request.await.map_err(Refusal::from)?;
        let now = Instant::now();
        control.source_age_upper_ms = Some(fresh_age(original, now)?);
        let elapsed = u64::try_from(
            now.checked_duration_since(began)
                .ok_or(Refusal::Expired)?
                .as_nanos()
                .div_ceil(1_000_000),
        )
        .map_err(|_| Refusal::Expired)?;
        let frame = observation.frame.as_mut().ok_or(Refusal::NotCurrent)?;
        frame.receipt_age_ms = frame
            .receipt_age_ms
            .checked_add(elapsed)
            .ok_or(Refusal::Expired)?;
        let metadata = Metadata::new(observation, image.snapshot(), Some(control))
            .map_err(|_| Refusal::NotCurrent)?;
        self.check_export(&metadata)?;
        Ok((metadata, image))
    }
    /// Recheck before sealing the local reply, including each resumed socket
    /// write poll. Concurrent STATUS invalidates this pin; it is never replaced.
    pub(crate) fn check_export(&self, metadata: &Metadata) -> Result<(), Refusal> {
        let mut state = self.locked();
        if state
            .stop
            .as_ref()
            .is_some_and(frd::session_startup::ViewerControl::is_stopped)
        {
            state.retire();
        }
        if state.stopped || state.ledger.is_closed() {
            return Err(Refusal::Closed);
        }
        if state.pending.is_some() {
            return Err(Refusal::Busy);
        }
        let control = metadata.control.as_ref().ok_or(Refusal::Unavailable)?;
        let (id, original) = state.pinned.ok_or(Refusal::NotCurrent)?;
        let view = original.view;
        if !control.ready
            || control.observation_id != Some(id)
            || control.lease.as_deref() != Some(super::super::hex(&state.lease).as_str())
            || control.next_request_id != state.ledger.consumed_through().checked_add(1)
            || control.frame != Some(original.sample.stamp.frame)
            || control.geometry_generation != Some(view.geometry.as_raw())
            || control.viewport_generation != Some(view.viewport.as_raw())
            || control.configuration_generation != Some(view.configuration.as_raw())
            || control.recovery_generation != Some(view.recovery.as_raw())
            || state.live.is_none_or(|live| live.view != view)
        {
            return Err(Refusal::NotCurrent);
        }
        fresh_age(original, Instant::now())?;
        Ok(())
    }
}
fn fresh_age(candidate: Candidate, now: Instant) -> Result<u64, Refusal> {
    candidate
        .age(now)
        .map(|age| age.div_ceil(1000))
        .filter(|age| *age < 1000)
        .ok_or(Refusal::Expired)
}

#[cfg(test)]
#[path = "screenshot_tests.rs"]
mod tests;
