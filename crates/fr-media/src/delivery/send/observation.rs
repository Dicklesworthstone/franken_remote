//! Source observations do not own codec references or renew repair retention.
use super::{OfferOrigin, PacketOffer, SendCache, SendError, deadline};
use fr_wire::{Channel, PipelineState, Progress, SourceObservation, encode_progress};

pub(super) struct PendingObservation {
    progress: Progress,
    pub(super) send_by: u64,
}
impl SendCache {
    /// Queue an actual full-source comparison of the LAST inserted picture.
    /// The capture adapter supplies evidence; a heartbeat must never call this.
    /// Retain one coalesced metadata value, not the encoded picture. A duplicate
    /// timestamp has no effect. The send deadline starts at observation, not at
    /// receipt, polling, packetization or the next transport-credit opportunity.
    pub fn observe_unchanged(
        &mut self,
        frame: u64,
        observed_micros: u64,
        now: u64,
    ) -> Result<bool, SendError> {
        self.tick(now)?;
        let previous = self.last_progress.ok_or(SendError::InvalidObservation)?;
        if previous.descriptor.frame != frame
            || observed_micros > now
            || observed_micros < previous.observed_micros
            || observed_micros < previous.descriptor.capture_micros
            || previous.observation == SourceObservation::Unknown
            || !matches!(
                previous.pipeline,
                PipelineState::Running | PipelineState::Idle
            )
        {
            return Err(SendError::InvalidObservation);
        }
        if observed_micros == previous.observed_micros {
            return Ok(false);
        }
        let send_by = deadline(observed_micros, self.policy.reference_horizon_micros)?;
        if now >= send_by {
            return Err(SendError::ObservationExpired);
        }
        let progress = Progress {
            observed_micros,
            observation: SourceObservation::QualifiedUnchanged,
            pipeline: PipelineState::Idle,
            ..previous
        };
        self.last_progress = Some(progress);
        self.observation = Some(PendingObservation { progress, send_by });
        Ok(true)
    }
    pub(super) fn next_observation(
        &mut self,
        out: &mut [u8],
    ) -> Result<Option<PacketOffer>, SendError> {
        let Some(pending) = &self.observation else {
            return Ok(None);
        };
        let byte_len = encode_progress(
            pending.progress,
            self.bindings.for_channel(Channel::MediaConfig),
            &self.limits,
            out,
        )?;
        let offer = PacketOffer {
            channel: Channel::MediaConfig,
            byte_len,
            frame: pending.progress.descriptor.frame,
            send_by_micros: pending.send_by,
            origin: OfferOrigin {
                owner: self.owner.clone(),
                epoch: self.epoch,
            },
        };
        self.observation = None;
        Ok(Some(offer))
    }
}
