//! Remote cursor forwarding on a single-view stream's DEDICATED capture source
//! (the controlled share), plan §11.4 and PROTOCOL.md `0x0038`/`0x0039`.
//!
//! The same bounded owners as the shared publisher: [`HostCursor`] maps native
//! cursor serials to never-reused wire IDs (16 shapes / 1 MiB) and ONE
//! [`HostLane`] sends each shape once on the reliable media-configuration lane,
//! then one replaceable position datagram. The capture producer samples the
//! cursor right after each capture, on the same capture credit and consent;
//! the network task sends between complete turns. A sample is confirmed OS
//! pointer state for rendering only: it touches no frame ID, source progress,
//! freshness, presentation or input evidence, and is never an input command.
use super::{Error, StreamingHost};
use crate::{
    media::{CaptureSource, ObservationControl},
    media_quic::{HostLane, NegotiatedMedia},
};
use asupersync::cx::Cx;
use fr_media::cursor::{HostCursor, SourceState};
use fr_transport::quic::QuicRecords;
use std::sync::Mutex;

struct State {
    host: HostCursor,
    lane: HostLane,
}
/// Present only when the viewer positively selected `remote-cursor`.
pub(super) struct StreamCursor {
    media: NegotiatedMedia,
    // Shared by the producer and network halves of ONE serve task, which are
    // polled sequentially: never contended and never held across an await.
    state: Mutex<State>,
}
impl StreamCursor {
    /// Sampling demand: the source can still observe a separate cursor.
    /// `Unsupported` (e.g. no XFIXES) and exhausted IDs are terminal.
    fn wanted(&self) -> Result<bool, Error> {
        let state = self.state.lock().map_err(|_| Error::Closed)?;
        Ok(state.host.state() == SourceState::Forwarding)
    }
    /// One bounded `ReadCursor` on the ORIGINAL capture worker under the same
    /// selected-source consent. A failed exchange poisons the worker exactly
    /// like a capture; typed cursor states never end the stream.
    pub(super) async fn sample(
        &self,
        source: &mut CaptureSource,
        control: &ObservationControl,
    ) -> Result<(), Error> {
        if !self.wanted()? {
            return Ok(());
        }
        let reply = source.read_cursor(control).await.map_err(Error::Media)?;
        let observation = reply.observation(source).map_err(Error::Media)?;
        let mut state = self.state.lock().map_err(|_| Error::Closed)?;
        // A refused image is hidden by the owner itself; not a capture failure.
        let _ = state.host.observe(&observation);
        let target = state.host.target();
        state.lane.lane.observe(target);
        Ok(())
    }
    /// At most one reliable shape and one position datagram, admitted only
    /// while this stream's observation authority holds at the send itself.
    pub(super) fn service(
        &self,
        cx: &Cx,
        q: &mut QuicRecords,
        control: &ObservationControl,
    ) -> Result<usize, Error> {
        let Some(lanes) = self.media.cursor_lanes(q).map_err(Error::MediaTransport)? else {
            return Ok(0);
        };
        let now = control.check().map_err(Error::Media)?.as_micros();
        let mut state = self.state.lock().map_err(|_| Error::Closed)?;
        let State { host, lane } = &mut *state;
        let turn = lane.service(cx, q, &lanes, host, now, &mut || control.check().is_ok())?;
        Ok(turn.accepted)
    }
}

impl StreamingHost {
    /// Forward the host cursor on THIS stream's exact negotiated media, before
    /// service. `Ok(false)` is typed absence: the viewer did not select
    /// `remote-cursor`, so no cursor record is sampled, sent or accepted.
    /// Observation-only recovery owns its media and does not forward a cursor.
    pub fn enable_cursor(&mut self, media: NegotiatedMedia) -> Result<bool, Error> {
        if self.stream.served || self.cursor.is_some() || self.recovery.is_some() {
            return Err(Error::Order);
        }
        let session = self.host.session()?;
        session.check()?;
        media
            .check(&session.opened.transport)
            .map_err(Error::MediaTransport)?;
        if media.binding()
            != self
                .stream
                .sender
                .feedback_view()
                .map_err(Error::MediaTransport)?
        {
            return Err(Error::Order);
        }
        if !media.cursor_selected() {
            return Ok(false);
        }
        self.cursor = Some(Box::new(StreamCursor {
            media,
            state: Mutex::new(State {
                host: HostCursor::new(),
                lane: HostLane::default(),
            }),
        }));
        Ok(true)
    }
}
