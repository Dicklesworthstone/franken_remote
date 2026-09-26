//! Remote cursor forwarding for the ONE original capture source (plan §11.4).
//!
//! The source samples the host's logical cursor at capture cadence, only while
//! at least one admitted, streaming viewer selected `remote-cursor`. Each such
//! viewer owns a [`ViewerLane`]: its current shape once on the reliable
//! media-configuration lane, then ONE replaceable position datagram. A viewer
//! in decoder startup, late join or recovery receives nothing. Sampling is
//! never capture freshness: it touches no frame ID, source progress, recovery
//! allowance or view readiness.
use super::{Entry, Error, Members, ObservationControl, Publisher, SendReport};
use asupersync::cx::Cx;
use fr_media::cursor::{HostCursor, SourceState};
use fr_transport::quic::QuicRecords;

/// Per-viewer cursor state, bound to the exact media view it was sent on.
pub(super) type EntryCursor = crate::media_quic::HostLane;

impl Entry {
    /// Admitted, steady observation on live original media only.
    fn cursor_streaming(&self) -> bool {
        self.failure.is_none()
            && self.join.is_none()
            && self.starting.is_none()
            && self.recovery.is_none()
            && self
                .media
                .as_ref()
                .is_some_and(crate::media_quic::NegotiatedMedia::cursor_selected)
    }
    /// At most one shape and one position per call. Typed refusals (an image
    /// larger than this viewer's reliable record bound, exhausted sequences)
    /// degrade to the fallback or to silence; they never end the viewer.
    pub(super) fn service_cursor(
        &mut self,
        cx: &Cx,
        transport: &mut QuicRecords,
        host: &HostCursor,
        owner: &ObservationControl,
        report: &mut SendReport,
    ) -> Result<(), Error> {
        if !self.cursor_streaming() {
            return Ok(());
        }
        let media = self.media.as_ref().ok_or(Error::Closed)?;
        let Some(lanes) = media.cursor_lanes(transport).map_err(Error::Transport)? else {
            return Ok(());
        };
        let now = owner.check().map_err(Error::Media)?.as_micros();
        let control = self.control.clone();
        let mut authorize = || owner.check().is_ok() && control.check().is_ok();
        let turn = self
            .cursor
            .service(cx, transport, &lanes, host, now, &mut authorize)
            .map_err(|e| Error::Transport(crate::media_quic::Error::Transport(e)))?;
        report.accepted += turn.accepted;
        report.pending |= turn.pending;
        Ok(())
    }
}

impl Members {
    fn cursor_demand(&self) -> bool {
        self.cursor.state() == SourceState::Forwarding
            && self.entries.iter().flatten().any(Entry::cursor_streaming)
    }
}

impl Publisher {
    /// One bounded cursor sample on the original source, between captures and
    /// only with real demand; a source without a separate cursor (typed
    /// `Unsupported`) is never polled again. Worker failure is terminal like
    /// a failed capture, but typed cursor states never end the publication.
    pub(super) async fn sample_cursor(&mut self) -> Result<(), Error> {
        let owner = {
            let mut members = self.members.lock().map_err(|_| Error::Poisoned)?;
            members.tick()?;
            if !members.cursor_demand() {
                return Ok(());
            }
            members.owner.clone()
        };
        let reply = self
            .source
            .read_cursor(&owner)
            .await
            .map_err(Error::Media)?;
        let observation = reply.observation(&self.source).map_err(Error::Media)?;
        let mut members = self.members.lock().map_err(|_| Error::Poisoned)?;
        members.tick()?;
        // A refused image is hidden by the owner itself; not a capture failure.
        let _ = members.cursor.observe(&observation);
        let target = members.cursor.target();
        for entry in members.entries.iter_mut().flatten() {
            if entry.cursor_streaming() {
                entry.cursor.lane.observe(target);
            }
        }
        Ok(())
    }
}
