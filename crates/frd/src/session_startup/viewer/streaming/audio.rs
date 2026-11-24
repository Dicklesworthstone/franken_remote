//! The viewer's end of `audio-down` (plan §15.4; PROTOCOL.md 0x0060-0x0063).
//!
//! frd links no audio library: the native client supplies an [`AudioOutput`]
//! (real libopus decode and `PulseAudio` output in `fr`). This owner validates
//! every record BEFORE the output sees it, accepts records only on the exact
//! negotiated audio routes, admits `AudioPacket`s only after its own
//! `AudioConfigured` was admitted by the transport, and treats `AudioStop` as
//! an immediate fence. Audio never touches the media receive pipeline,
//! presentation, freshness or input.
use super::Error;
use crate::media_quic::{AudioLanes, NegotiatedMedia};
use asupersync::cx::Cx;
use fr_core::audio::{AudioDirection, AudioGeneration, AudioStopReason};
use fr_transport::quic::{self, QuicRecords, Route};
use fr_wire::audio::{self as wire, AudioConfiguration};

mod acknowledgement;
mod reply;

/// Reliable-record retention for this viewer's replies.
const REPLY_SEND_US: u64 = 2_000_000;

pub use fr_client::audio::output::{AudioEnd, AudioOutput, OutputRefused, Unavailable};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Configuring(AudioConfiguration),
    Active(AudioConfiguration),
    Ended,
}

/// Content-free viewer-side accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioStatistics {
    pub packets: u64,
    /// Packets outside an acknowledged configuration: dropped, never played.
    pub dropped: u64,
    /// Local output resets that asked the host for a fresh epoch.
    pub restarts: u64,
}

pub(crate) struct ViewerAudio {
    lanes: AudioLanes,
    output: Option<Box<dyn AudioOutput>>,
    state: State,
    last: Option<AudioGeneration>,
    /// A local stop to send to the host before anything else.
    stop: Option<(AudioGeneration, AudioStopReason)>,
    statistics: AudioStatistics,
    /// Local output resets without a healthy generation before audio ends
    /// (typed `Local`): the host lane's own bound, `MAX_LANE_RESTARTS`.
    budget: fr_media::audio_delivery::RestartBudget,
    pending_reply: Option<reply::Pending>,
}
impl std::fmt::Debug for ViewerAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewerAudio")
            .field("state", &self.state)
            .field("statistics", &self.statistics)
            .finish_non_exhaustive()
    }
}
impl ViewerAudio {
    /// `Ok(None)`: audio-down was not selected/attached (typed absence).
    pub(super) fn attach(media: &NegotiatedMedia, q: &QuicRecords) -> Result<Option<Self>, Error> {
        let Some(lanes) = media.audio_lanes(q).map_err(Error::Routes)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            lanes,
            output: None,
            state: State::Idle,
            last: None,
            stop: None,
            statistics: AudioStatistics::default(),
            budget: fr_media::audio_delivery::RestartBudget::new(),
            pending_reply: None,
        }))
    }
    /// Install the local output once, before the first configuration.
    pub(crate) fn configure(&mut self, output: Box<dyn AudioOutput>) -> Result<(), Unavailable> {
        if self.output.is_some() || self.state != State::Idle {
            return Err(Unavailable::AlreadyConfigured);
        }
        self.output = Some(output);
        Ok(())
    }
    /// Local session retirement is not a host `AudioStop` or a playback failure.
    /// Drop the native output immediately, retaining content-free counters. Do
    /// not poll it, flush queued samples, acknowledge configuration or renew it.
    pub(super) fn close(&mut self) {
        self.state = State::Ended;
        self.stop = None;
        self.pending_reply = None;
        drop(self.output.take());
    }
    pub(crate) const fn statistics(&self) -> AudioStatistics {
        self.statistics
    }
    pub(super) fn owns(&self, route: Route, bytes: &[u8]) -> bool {
        let kind = wire::record_kind(bytes);
        (route == Route::Stream(self.lanes.control) && matches!(kind, Some(0x0060 | 0x0063)))
            || (route == Route::Datagram(self.lanes.packets) && kind == Some(0x0062))
    }
    fn end(&mut self, end: AudioEnd) {
        self.state = State::Ended;
        self.pending_reply = None;
        if let Some(output) = &mut self.output {
            output.ended(end);
        }
    }
    fn retired(&mut self, end: AudioEnd) {
        self.end(end);
        self.stop = None;
        // Drop is the required nonblocking playback/decoder fence. Its native
        // owner retains any independently observed cleanup result.
        drop(self.output.take());
    }
    /// The local output failed: tell the host (`DeviceChanged` = reset me),
    /// drop the failed epoch's device/decoder at once, and wait for a fresh
    /// epoch. Old samples are never replayed into the next output.
    fn fail_locally(&mut self, generation: AudioGeneration) {
        self.pending_reply = None;
        self.stop = Some((generation, AudioStopReason::DeviceChanged));
        if self.budget.spend(self.statistics.packets) {
            self.statistics.restarts += 1;
            self.state = State::Idle;
            if let Some(output) = &mut self.output {
                output.reset();
            }
        } else {
            self.end(AudioEnd::Local);
        }
    }
    /// One record on an owned route. Malformed or out-of-state host records
    /// are refused before the output sees any payload; they fence audio only.
    pub(super) fn receive(&mut self, route: Route, bytes: &[u8], live: &mut dyn FnMut() -> bool) {
        if route == Route::Datagram(self.lanes.packets) {
            let State::Active(offer) = self.state else {
                self.statistics.dropped += 1;
                return;
            };
            // Full validation (bounds, binding, generation) before the output.
            let valid = wire::decode_packet(bytes, self.lanes.binding).is_ok_and(|p| {
                p.direction == AudioDirection::Downlink
                    && p.generation == offer.generation
                    && p.payload.len() <= usize::try_from(offer.max_packet_bytes).unwrap_or(0)
                    && u32::from(p.duration_samples) <= offer.max_decoded_samples
            });
            if !valid {
                self.statistics.dropped += 1;
                return;
            }
            self.statistics.packets += 1;
            let delivered = self
                .output
                .as_mut()
                .is_some_and(|o| o.receive(bytes, live).is_ok());
            if !delivered {
                self.fail_locally(offer.generation);
            }
            return;
        }
        match wire::record_kind(bytes) {
            Some(0x0060) => self.configuration(bytes),
            Some(0x0063) => self.host_stop(bytes, live),
            _ => self.end(AudioEnd::Local),
        }
    }
    fn configuration(&mut self, bytes: &[u8]) {
        if self.state == State::Ended {
            // Terminal for this session: nothing reopens it.
            return;
        }
        let Ok(offer) = wire::decode_configuration(bytes, self.lanes.binding) else {
            self.end(AudioEnd::Local);
            return;
        };
        // Downlink only, strictly newer generation, never mid-stream.
        if offer.direction != AudioDirection::Downlink
            || self
                .last
                .is_some_and(|g| offer.generation.as_raw() <= g.as_raw())
            || matches!(self.state, State::Configuring(_) | State::Active(_))
        {
            self.end(AudioEnd::Local);
            return;
        }
        self.last = Some(offer.generation);
        let Some(output) = &mut self.output else {
            // Typed refusal to the host: nothing will play here.
            self.stop = Some((offer.generation, AudioStopReason::UserMute));
            self.end(AudioEnd::NoOutput);
            return;
        };
        if output.configure(self.lanes.binding, offer).is_err() {
            self.fail_locally(offer.generation);
            return;
        }
        self.state = State::Configuring(offer);
    }
    fn host_stop(&mut self, bytes: &[u8], live: &mut dyn FnMut() -> bool) {
        let Ok(stop) = wire::decode_stop(bytes, self.lanes.binding) else {
            self.end(AudioEnd::Local);
            return;
        };
        if stop.direction != AudioDirection::Downlink {
            return;
        }
        match self.state {
            State::Configuring(offer) | State::Active(offer)
                if offer.generation == stop.generation =>
            {
                // The output fences its decoder queue before any other work.
                if let Some(output) = &mut self.output {
                    let _ = output.receive(bytes, live);
                }
                self.end(AudioEnd::Host(stop.reason));
            }
            // A source that failed before any configuration reached us.
            State::Idle
                if self
                    .last
                    .is_none_or(|g| stop.generation.as_raw() > g.as_raw()) =>
            {
                self.last = Some(stop.generation);
                self.end(AudioEnd::Host(stop.reason));
            }
            // Stale generation: refused, nothing changes.
            _ => {}
        }
    }
    /// Native replies enter one bounded slot; only actual transport admission
    /// enables packets. Congestion neither resets the output nor buys more time.
    pub(super) fn service(
        &mut self,
        q: &mut QuicRecords,
        cx: &Cx,
        now_us: u64,
        live: &mut dyn FnMut() -> bool,
    ) -> Result<(), Error> {
        if !self.check_pair(q, cx, live)? {
            return Ok(());
        }
        let inbound = self.lanes.control;
        self.service_reply(
            now_us,
            &mut || cx.checkpoint().is_ok() && live(),
            &mut |route, bytes, until, permitted| match q.send(cx, route, bytes, until, permitted) {
                Err(quic::Error::WrongRoute | quic::Error::Expired)
                    if q.audio_receive_expired(inbound) == Ok(true) =>
                {
                    Err(reply::SendFailure::ReceiveExpired)
                }
                Err(quic::Error::WrongRoute | quic::Error::Expired)
                    if q.audio_send_expired(inbound) == Ok(true) =>
                {
                    Err(reply::SendFailure::SendExpired)
                }
                result => result.map_err(reply::SendFailure::Transport),
            },
        )?;
        // Native completion can cross the deadline even without a reply. The
        // original pair must still be usable before this turn returns.
        self.check_pair(q, cx, live)?;
        Ok(())
    }
    fn check_pair(
        &mut self,
        q: &mut QuicRecords,
        cx: &Cx,
        live: &mut dyn FnMut() -> bool,
    ) -> Result<bool, Error> {
        // Retire expired framing and retained sends before any output callback.
        // A missing route cannot leave playback alive or turn its pending
        // acknowledgement into a WrongRoute failure of the desktop session.
        if let Err(error) = q.tick(cx, &mut *live) {
            self.close();
            return Err(Error::Transport(error));
        }
        let expired = match q
            .audio_receive_expired(self.lanes.control)
            .and_then(|receive| {
                q.audio_send_expired(self.lanes.control).map(|send| {
                    if receive {
                        Some(AudioEnd::ReceiveExpired)
                    } else if send {
                        Some(AudioEnd::SendExpired)
                    } else {
                        None
                    }
                })
            }) {
            Ok(expired) => expired,
            Err(error) => {
                self.close();
                return Err(Error::Transport(error));
            }
        };
        let routes_missing = !q.has_route(Route::Stream(self.lanes.control))
            || !q.has_route(Route::Stream(self.lanes.replies))
            || !q.has_route(Route::Datagram(self.lanes.packets));
        let ended = if expired.is_some() || routes_missing {
            true
        } else {
            match q.receive_ended(self.lanes.control) {
                Ok(ended) => ended,
                Err(error) => {
                    self.close();
                    return Err(Error::Transport(error));
                }
            }
        };
        if ended {
            self.retired(expired.unwrap_or(AudioEnd::ChannelClosed));
            return Ok(false);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests;

impl Drop for ViewerAudio {
    fn drop(&mut self) {
        self.close();
    }
}
