//! Admission of one completed private audio pull into the production ring.
//!
//! The newest packet is anchored at the ORIGINAL host request, not reply time.
//! Earlier packets retain their sample-timeline distance, including holes.
//! This is a conservative admission policy, NOT a physical capture timestamp:
//! it bounds the worker exchange but does not certify pre-pull device latency.
//! No worker clock is subtracted from the host clock, and sound never proves
//! freshness of a displayed view or grants input permission.
use fr_core::audio::{AudioDirection, OPUS_SAMPLE_RATE};
use fr_media::{
    audio_delivery::{AudioRing, MAX_PACKET_AGE_US, SourceState, SourceStream},
    worker::audio::{Batch, MAX_BATCH_PACKETS},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    Clock,
    Packet,
    Ordering,
    Ring,
}

/// One source generation. Consumed identities survive dropped/expired batches;
/// a later pull cannot replay them with a new local admission anchor.
#[derive(Debug)]
pub(super) struct Admission {
    stream: SourceStream,
    last: Option<(u64, u64)>,
    admitted: u64,
    obsolete: u64,
}
impl Admission {
    pub(super) const fn new(stream: SourceStream) -> Self {
        Self {
            stream,
            last: None,
            admitted: 0,
            obsolete: 0,
        }
    }

    /// Validate the WHOLE bounded batch before changing the ring. A late but
    /// valid batch advances the consumed floor, counts its losses and allows
    /// the same source to continue on a subsequent timely pull.
    pub(super) fn admit(
        &mut self,
        batch: Batch,
        ring: &mut AudioRing,
        requested: u64,
        completed: u64,
    ) -> Result<(), Error> {
        if completed < requested || requested.checked_add(MAX_PACKET_AGE_US).is_none() {
            return Err(Error::Clock);
        }
        if ring.state() != SourceState::Live(self.stream) {
            return Err(Error::Ring);
        }
        if batch.packets.len() > MAX_BATCH_PACKETS {
            return Err(Error::Packet);
        }
        let mut previous = self.last;
        let frame = u64::from(self.stream.frame_duration_ms) * u64::from(OPUS_SAMPLE_RATE / 1000);
        for unit in &batch.packets {
            if unit.generation() != self.stream.generation
                || unit.direction() != AudioDirection::Downlink
                || u64::from(unit.duration_samples()) != frame
                || unit.payload().len() > usize::from(self.stream.max_packet_bytes)
            {
                return Err(Error::Packet);
            }
            let end = unit
                .timestamp_samples()
                .checked_add(frame)
                .ok_or(Error::Packet)?;
            if previous.is_some_and(|(sequence, until)| {
                unit.sequence() <= sequence || unit.timestamp_samples() < until
            }) {
                return Err(Error::Ordering);
            }
            previous = Some((unit.sequence(), end));
        }
        let Some(newest) = batch.packets.last() else {
            return Ok(());
        };
        let newest = newest.timestamp_samples();
        let mut anchors = [None; MAX_BATCH_PACKETS];
        for (anchor, unit) in anchors.iter_mut().zip(&batch.packets) {
            let distance = newest
                .checked_sub(unit.timestamp_samples())
                .ok_or(Error::Ordering)?;
            let age = distance
                .checked_mul(1_000_000)
                .ok_or(Error::Clock)?
                .div_ceil(u64::from(OPUS_SAMPLE_RATE));
            // An unrepresentable pre-origin timestamp is discarded. Saturating
            // to zero would give it a made-up deadline near host startup.
            *anchor = requested.checked_sub(age).filter(|&at| {
                at.checked_add(MAX_PACKET_AGE_US)
                    .is_some_and(|until| completed < until)
            });
        }
        self.last = previous;
        for (unit, anchor) in batch.packets.into_iter().zip(anchors) {
            if let Some(anchor) = anchor {
                ring.push(unit, anchor).map_err(|_| Error::Ring)?;
                self.admitted = self.admitted.saturating_add(1);
            } else {
                self.obsolete = self.obsolete.saturating_add(1);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
