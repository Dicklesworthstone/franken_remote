//! One retained control reply, never a queue of samples or re-created epochs.
//! Native completion transfers custody here; transport admission is separate.
use super::{
    AudioConfiguration, AudioDirection, AudioEnd, AudioGeneration, AudioStopReason, Error,
    OutputRefused, REPLY_SEND_US, Route, State, ViewerAudio, acknowledgement, quic, wire,
};

const CAPACITY: usize = if wire::AUDIO_CONFIGURED_RECORD_BYTES > wire::AUDIO_STOP_RECORD_BYTES {
    wire::AUDIO_CONFIGURED_RECORD_BYTES
} else {
    wire::AUDIO_STOP_RECORD_BYTES
};

#[derive(Clone, Copy)]
enum Completion {
    Configured(AudioConfiguration),
    Stop(AudioGeneration, AudioStopReason),
}
pub(super) struct Pending {
    bytes: [u8; CAPACITY],
    len: usize,
    created: u64,
    until: u64,
    completion: Completion,
}
impl Pending {
    fn new(bytes: &[u8], completion: Completion, created: u64) -> Result<Self, OutputRefused> {
        let expected = match completion {
            Completion::Configured(_) => wire::AUDIO_CONFIGURED_RECORD_BYTES,
            Completion::Stop(..) => wire::AUDIO_STOP_RECORD_BYTES,
        };
        if bytes.len() != expected {
            return Err(OutputRefused);
        }
        let mut pending = Self {
            bytes: [0; CAPACITY],
            len: bytes.len(),
            created,
            until: created.checked_add(REPLY_SEND_US).ok_or(OutputRefused)?,
            completion,
        };
        pending.bytes[..bytes.len()].copy_from_slice(bytes);
        Ok(pending)
    }
    fn live_at(&self, now: u64) -> bool {
        self.created <= now && now < self.until
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

impl ViewerAudio {
    /// The same production service with an injected send boundary for bounded,
    /// deterministic congestion tests. The caller retains connection ownership.
    pub(super) fn service_reply(
        &mut self,
        now: u64,
        live: &mut dyn FnMut() -> bool,
        send: &mut impl FnMut(Route, &[u8], u64, &mut dyn FnMut() -> bool) -> Result<(), quic::Error>,
    ) -> Result<(), Error> {
        if !live() {
            self.close();
            return Err(Error::Transport(quic::Error::Unauthorized));
        }
        if let Some((generation, reason)) = self.stop {
            if self.pending_reply.is_none() {
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
                let Ok(pending) = Pending::new(&record, Completion::Stop(generation, reason), now)
                else {
                    self.reply_failed();
                    return Ok(());
                };
                self.pending_reply = Some(pending);
            }
            if !self.flush_reply(now, live, send)? || self.output.is_none() {
                return Ok(());
            }
        }
        if self.pending_reply.as_ref().is_some_and(|p| !p.live_at(now)) {
            self.reply_failed();
            return Ok(());
        }
        let offer = match self.state {
            State::Configuring(offer) | State::Active(offer) => Some(offer),
            State::Idle | State::Ended => None,
        };
        let Some(output) = &mut self.output else {
            return Ok(());
        };
        // An output must emit its acknowledgement once, including while the
        // previous turn's reply awaits transport credit. A second callback is
        // not backpressure and cannot hide behind the first valid reply.
        let state = if self.pending_reply.is_some() {
            State::Idle
        } else {
            self.state
        };
        let mut gate = acknowledgement::Gate::new(state, self.lanes.binding);
        let pending = &mut self.pending_reply;
        let mut custody_failed = false;
        let result = {
            let permission = std::cell::RefCell::new(&mut *live);
            output.service(&mut || (permission.borrow_mut())(), &mut |record| {
                gate.admit(record, || {
                    if !(permission.borrow_mut())() {
                        return Err(OutputRefused);
                    }
                    let offer = offer.ok_or(OutputRefused)?;
                    let retained = Pending::new(record, Completion::Configured(offer), now)
                        .inspect_err(|_| {
                            custody_failed = true;
                        })?;
                    *pending = Some(retained);
                    Ok(())
                })
            })
        };
        // A native callback is never authority, even if it swallows refusal.
        if !live() {
            self.close();
            return Err(Error::Transport(quic::Error::Unauthorized));
        }
        if custody_failed {
            self.reply_failed();
            return Ok(());
        }
        if result.is_err() || gate.failed() {
            if let Some(offer) = offer {
                self.fail_locally(offer.generation);
            } else if gate.failed() {
                self.reply_failed();
            }
            return Ok(());
        }
        if gate.admitted() || self.pending_reply.is_some() {
            self.flush_reply(now, live, send)?;
        }
        Ok(())
    }

    fn flush_reply(
        &mut self,
        now: u64,
        live: &mut dyn FnMut() -> bool,
        send: &mut impl FnMut(Route, &[u8], u64, &mut dyn FnMut() -> bool) -> Result<(), quic::Error>,
    ) -> Result<bool, Error> {
        if !live() {
            self.close();
            return Err(Error::Transport(quic::Error::Unauthorized));
        }
        let Some(pending) = &self.pending_reply else {
            return Ok(false);
        };
        let matches = match pending.completion {
            Completion::Configured(offer) => {
                self.state == State::Configuring(offer) && self.last == Some(offer.generation)
            }
            Completion::Stop(generation, reason) => self.stop == Some((generation, reason)),
        };
        if !matches || !pending.live_at(now) {
            // Do not pass an expired optional reply into the shared transport,
            // regenerate it, or spend another decoder restart to buy more time.
            self.reply_failed();
            return Ok(false);
        }
        match send(
            Route::Stream(self.lanes.replies),
            &pending.bytes[..pending.len],
            pending.until,
            live,
        ) {
            Ok(()) => {
                let pending = self.pending_reply.take().expect("original reply admitted");
                match pending.completion {
                    Completion::Configured(offer) => {
                        self.state = State::Active(offer);
                        if self.output.as_mut().is_none_or(|output| {
                            output
                                .configuration_admitted(self.lanes.binding, offer)
                                .is_err()
                        }) {
                            self.fail_locally(offer.generation);
                        }
                    }
                    Completion::Stop(..) => self.stop = None,
                }
                Ok(true)
            }
            Err(quic::Error::Backpressure) => Ok(false),
            Err(error) => {
                self.close();
                Err(Error::Transport(error))
            }
        }
    }
    fn reply_failed(&mut self) {
        self.end(AudioEnd::Local);
        self.stop = None;
        // No native cleanup or queued playback may keep this failed lane live.
        // Drop is the nonblocking fence required by AudioOutput's contract.
        drop(self.output.take());
    }
}
