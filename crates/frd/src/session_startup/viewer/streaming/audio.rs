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

/// Reliable-record retention for this viewer's replies.
const REPLY_SEND_US: u64 = 2_000_000;

/// Why no (more) audio plays; content-free and typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEnd {
    /// The host stopped (or never started) its source, with its reason.
    Host(AudioStopReason),
    /// No local output was configured for a selected channel.
    NoOutput,
    /// The local output failed or refused; the host was told to stop.
    Local,
}

/// The local output refused or failed; audio ends, video continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputRefused;

/// Why a local output cannot be installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// The host did not select audio-down or no channel attached (typed
    /// absence: not locally enabled there, or a controlled session).
    NotSelected,
    /// An output is installed already, or the viewer already started serving.
    AlreadyConfigured,
}

/// Local playback supplied by the native client. Calls are synchronous,
/// bounded and nonblocking; `live` rechecks this session's observation at
/// each native boundary. Implementations must not log PCM or payload bytes.
pub trait AudioOutput {
    /// Begin configuring the local output for this validated downlink offer.
    fn configure(&mut self, binding: u32, offer: AudioConfiguration) -> Result<(), OutputRefused>;
    /// Bounded progress: startup, then decode/render. Once the local output
    /// and decoder are ready, pass exactly one `AudioConfigured` record to
    /// `acknowledge`; its `Ok` is local send admission, not remote receipt.
    fn service(
        &mut self,
        live: &mut dyn FnMut() -> bool,
        acknowledge: &mut dyn FnMut(&[u8]) -> Result<(), OutputRefused>,
    ) -> Result<(), OutputRefused>;
    /// One validated `AudioPacket` or matching `AudioStop` record.
    fn receive(
        &mut self,
        bytes: &[u8],
        live: &mut dyn FnMut() -> bool,
    ) -> Result<(), OutputRefused>;
    /// Typed end; the output stops and flushes its own device.
    fn ended(&mut self, end: AudioEnd);
    /// The local output failed (a timing miss, expired or malformed audio):
    /// drop its device and decoder NOW, without flushing, and accept one
    /// later `configure` for a strictly newer host epoch.
    fn reset(&mut self);
}

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

/// Local output resets per session before audio ends (typed `Local`). Equal
/// to the host lane's own bound.
pub const MAX_RESTARTS: u64 = fr_media::audio_delivery::MAX_LANE_RESTARTS;

pub(crate) struct ViewerAudio {
    lanes: AudioLanes,
    output: Option<Box<dyn AudioOutput>>,
    state: State,
    last: Option<AudioGeneration>,
    /// A local stop to send to the host before anything else.
    stop: Option<(AudioGeneration, AudioStopReason)>,
    statistics: AudioStatistics,
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
        if let Some(output) = &mut self.output {
            output.ended(end);
        }
    }
    /// The local output failed: tell the host (`DeviceChanged` = reset me),
    /// drop the failed epoch's device/decoder at once, and wait for a fresh
    /// epoch. Old samples are never replayed into the next output.
    fn fail_locally(&mut self, generation: AudioGeneration) {
        self.stop = Some((generation, AudioStopReason::DeviceChanged));
        if self.statistics.restarts < MAX_RESTARTS {
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
    /// Bounded: output progress, the one acknowledgement, a pending stop.
    pub(super) fn service(
        &mut self,
        q: &mut QuicRecords,
        cx: &Cx,
        now_us: u64,
        live: &mut dyn FnMut() -> bool,
    ) -> Result<(), Error> {
        if let Some((generation, reason)) = self.stop {
            let mut record = [0; wire::AUDIO_STOP_RECORD_BYTES];
            wire::encode_stop(
                &wire::AudioStop {
                    direction: AudioDirection::Downlink,
                    generation,
                    reason,
                },
                self.lanes.binding,
                &mut record,
            )
            .map_err(Error::Wire)?;
            match q.send(
                cx,
                Route::Stream(self.lanes.replies),
                &record,
                now_us.saturating_add(REPLY_SEND_US),
                &mut *live,
            ) {
                Ok(()) => self.stop = None,
                Err(quic::Error::Backpressure) => return Ok(()),
                Err(error) => return Err(Error::Transport(error)),
            }
        }
        // After an end the output still gets bounded turns to finish its own
        // device cork/flush; it can never acknowledge or play again.
        let offer = match self.state {
            State::Configuring(offer) | State::Active(offer) => Some(offer),
            State::Idle | State::Ended => None,
        };
        let Some(output) = &mut self.output else {
            return Ok(());
        };
        let replies = Route::Stream(self.lanes.replies);
        let mut acknowledged = false;
        let mut transport_error = None;
        let result = {
            let mut acknowledge = |record: &[u8]| {
                if !matches!(self.state, State::Configuring(_)) {
                    return Err(OutputRefused);
                }
                // The session's own context is cancelled on close/revocation.
                match q.send(
                    cx,
                    replies,
                    record,
                    now_us.saturating_add(REPLY_SEND_US),
                    || cx.checkpoint().is_ok(),
                ) {
                    Ok(()) => {
                        acknowledged = true;
                        Ok(())
                    }
                    Err(error) => {
                        transport_error = Some(error);
                        Err(OutputRefused)
                    }
                }
            };
            output.service(live, &mut acknowledge)
        };
        if let Some(offer) = offer {
            if acknowledged {
                self.state = State::Active(offer);
            }
            if result.is_err() || transport_error.is_some() {
                self.fail_locally(offer.generation);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
