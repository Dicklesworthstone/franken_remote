//! The exclusive publisher's subscription to the SAME bounded audio source.
//! There is no second codec loop, packet queue, epoch machine or renewal owner.
use super::{
    AudioFeed, AudioRing, EntryAudio, Error, FeedTarget, ObservationControl, SendReport,
};
use crate::media_quic::{AudioLanes, NegotiatedMedia};
use fr_core::audio::AudioStopReason;
use fr_transport::quic::{ConnectionBinding, Disposition, QuicRecords, Route};
use std::sync::{Arc, Mutex};

pub(super) struct State {
    pub(super) owner: ObservationControl,
    pub(super) ring: AudioRing,
    pub(super) audio: EntryAudio,
    pub(super) closed: bool,
    connection: ConnectionBinding,
    lanes: AudioLanes,
}
impl State {
    fn close(&mut self) {
        self.closed = true;
        self.audio.close();
        self.ring.end(AudioStopReason::SessionEnded);
    }
}
impl AudioFeed {
    /// Join a completed native publisher's audio attachment. No worker starts
    /// here. Absence is not permission to synthesize routes or widen negotiation.
    /// The supplied control is the publisher's ORIGINAL observation authority.
    pub fn exclusive(
        owner: ObservationControl,
        media: &NegotiatedMedia,
        q: &QuicRecords,
    ) -> Result<Option<Self>, Error> {
        owner.check().map_err(Error::Media)?;
        let Some(lanes) = media.audio_lanes(q).map_err(Error::Transport)? else {
            return Ok(None);
        };
        if !lanes.control.outbound || lanes.replies.outbound || !lanes.packets.outbound {
            return Err(Error::ForeignConnection);
        }
        Ok(Some(Self {
            target: FeedTarget::Exclusive(Arc::new(Mutex::new(State {
                owner,
                ring: AudioRing::new(),
                audio: EntryAudio {
                    lanes: Some(lanes),
                    ..EntryAudio::default()
                },
                closed: false,
                connection: q.binding(),
                lanes,
            }))),
        }))
    }
    /// Bounded synchronous transport work only. A retired audio route fences
    /// audio, not the input lease or the video pipeline. A foreign connection
    /// is refused before any of its queues can be touched.
    pub fn service_exclusive(&self, q: &mut QuicRecords) -> Result<SendReport, Error> {
        let FeedTarget::Exclusive(shared) = &self.target else {
            return Err(Error::WrongSource);
        };
        let mut state = shared.lock().map_err(|_| Error::Poisoned)?;
        let mut report = SendReport {
            accepted: 0,
            pending: false,
        };
        if state.closed {
            return Ok(report);
        }
        if !q.is_bound_to(&state.connection) {
            return Err(Error::ForeignConnection);
        }
        let lanes = state.lanes;
        if q.is_closed()
            || !q.has_route(Route::Stream(lanes.control))
            || !q.has_route(Route::Stream(lanes.replies))
            || !q.has_route(Route::Datagram(lanes.packets))
            || q.receive_ended(lanes.replies).unwrap_or(true)
        {
            state.close();
            return Ok(report);
        }
        let owner = state.owner.clone();
        owner.check().map_err(Error::Media)?;
        let State { ring, audio, .. } = &mut *state;
        audio.service(
            &owner.context(),
            q,
            ring,
            lanes,
            &owner,
            &owner,
            &mut report,
        )?;
        Ok(report)
    }
    /// Dispatch from the ORIGINAL session only, after its connection/authority
    /// checks. A stopped or invalidated subscription never consumes another
    /// application's route, and stale records cannot reopen its lane.
    pub fn receive_exclusive(
        &self,
        route: Route,
        bytes: &[u8],
    ) -> Result<Option<Disposition>, Error> {
        let FeedTarget::Exclusive(shared) = &self.target else {
            return Err(Error::WrongSource);
        };
        let mut state = shared.lock().map_err(|_| Error::Poisoned)?;
        if state.closed {
            return Ok(None);
        }
        state.owner.check().map_err(Error::Media)?;
        let State { ring, audio, .. } = &mut *state;
        Ok(audio.receive(ring, route, bytes))
    }
    /// Fence the optional lane without revoking input. The containing publisher
    /// separately revokes its original session FIRST on session termination.
    /// The source observes this fence and owns its own bounded worker cleanup.
    pub fn close_exclusive(&self) {
        if let FeedTarget::Exclusive(shared) = &self.target {
            shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .close();
        }
    }
}

#[cfg(test)]
mod tests;
