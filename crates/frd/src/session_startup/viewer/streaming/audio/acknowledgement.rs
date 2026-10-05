//! The session, not a native output callback, owns configuration acceptance.
//! A worker completion is neither authority nor transport send admission.
use super::{AudioConfiguration, OutputRefused, State, wire};

/// One service turn's single-use acknowledgement gate. State persists in the
/// owning `ViewerAudio` after this turn; only `Configuring` admits a first reply.
/// Keep failure independently of the output's return value: it may incorrectly
/// swallow a callback error. No payload retention or native work lives here.
pub(super) struct Gate {
    offer: Option<AudioConfiguration>,
    binding: u32,
    admitted: bool,
    failed: bool,
}
impl Gate {
    pub(super) fn new(state: State, binding: u32) -> Self {
        Self {
            offer: match state {
                State::Configuring(offer) => Some(offer),
                State::Idle | State::Active(_) | State::Ended => None,
            },
            binding,
            admitted: false,
            failed: false,
        }
    }
    pub(super) const fn admitted(&self) -> bool {
        self.admitted
    }
    pub(super) const fn failed(&self) -> bool {
        self.failed
    }
    pub(super) fn admit(
        &mut self,
        record: &[u8],
        send: impl FnOnce() -> Result<(), OutputRefused>,
    ) -> Result<(), OutputRefused> {
        let valid = self.offer.is_some_and(|offer| {
            wire::decode_configured(record, self.binding).is_ok_and(|ack| {
                ack.accepted
                    && ack.direction == offer.direction
                    && ack.generation == offer.generation
                    && ack.actual_channels == offer.channels
                    && ack.actual_sample_rate == offer.sample_rate
                    && ack.actual_frame_duration_ms == offer.frame_duration_ms
            })
        });
        if self.admitted || self.failed || !valid {
            self.failed = true;
            return Err(OutputRefused);
        }
        // Only a successful send through the original authenticated route can
        // open packet admission. A prepared/queued worker reply is not enough.
        match send() {
            Ok(()) => {
                self.admitted = true;
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::audio::{
        AudioChannels, AudioDirection, AudioGeneration, MAX_DECODED_SAMPLES, OPUS_SAMPLE_RATE,
    };

    const BINDING: u32 = 17;

    fn offer() -> AudioConfiguration {
        AudioConfiguration {
            direction: AudioDirection::Downlink,
            generation: AudioGeneration::from_raw(4),
            channels: AudioChannels::Stereo,
            sample_rate: OPUS_SAMPLE_RATE,
            frame_duration_ms: 20,
            max_packet_bytes: 64,
            max_decoded_samples: MAX_DECODED_SAMPLES,
            jitter_target_ms: 20,
        }
    }
    fn acknowledgement() -> wire::AudioConfigured {
        let offer = offer();
        wire::AudioConfigured {
            direction: offer.direction,
            generation: offer.generation,
            accepted: true,
            actual_channels: offer.channels,
            actual_sample_rate: offer.sample_rate,
            actual_frame_duration_ms: offer.frame_duration_ms,
        }
    }
    fn record(ack: wire::AudioConfigured, binding: u32) -> Vec<u8> {
        let mut bytes = vec![0; wire::AUDIO_CONFIGURED_RECORD_BYTES];
        wire::encode_configured(&ack, binding, &mut bytes).unwrap();
        bytes
    }
    fn gate() -> Gate {
        Gate::new(State::Configuring(offer()), BINDING)
    }
    fn assert_refused(gate: &mut Gate, bytes: &[u8]) {
        assert_eq!(
            gate.admit(bytes, || panic!("invalid reply reached transport")),
            Err(OutputRefused)
        );
        assert!(gate.failed());
    }

    #[test]
    fn exact_current_reply_admits_only_after_transport_accepts() {
        let mut gate = gate();
        let mut sends = 0;
        assert!(!gate.admitted());
        gate.admit(&record(acknowledgement(), BINDING), || {
            sends += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(sends, 1);
        assert!(gate.admitted());
        assert!(!gate.failed());
    }

    #[test]
    fn old_worker_epoch_never_reaches_transport() {
        let mut ack = acknowledgement();
        ack.generation = AudioGeneration::from_raw(3);
        let mut gate = gate();
        assert_refused(&mut gate, &record(ack, BINDING));
        assert!(!gate.admitted());
    }

    #[test]
    fn unannounced_future_epoch_never_reaches_transport() {
        let mut ack = acknowledgement();
        ack.generation = AudioGeneration::from_raw(5);
        assert_refused(&mut gate(), &record(ack, BINDING));
    }

    #[test]
    fn another_channel_cannot_acknowledge_this_offer() {
        assert_refused(&mut gate(), &record(acknowledgement(), BINDING + 1));
    }

    #[test]
    fn uplink_reply_cannot_open_downlink_admission() {
        let mut ack = acknowledgement();
        ack.direction = AudioDirection::Uplink;
        assert_refused(&mut gate(), &record(ack, BINDING));
    }

    #[test]
    fn refused_configuration_cannot_become_active() {
        let mut ack = acknowledgement();
        ack.accepted = false;
        assert_refused(&mut gate(), &record(ack, BINDING));
    }

    #[test]
    fn different_channel_count_cannot_become_active() {
        let mut ack = acknowledgement();
        ack.actual_channels = AudioChannels::Mono;
        assert_refused(&mut gate(), &record(ack, BINDING));
    }

    #[test]
    fn different_packet_duration_cannot_become_active() {
        let mut ack = acknowledgement();
        ack.actual_frame_duration_ms = 10;
        assert_refused(&mut gate(), &record(ack, BINDING));
    }

    #[test]
    fn malformed_or_truncated_reply_never_reaches_transport() {
        let bytes = record(acknowledgement(), BINDING);
        for length in [0, 1, bytes.len() - 1] {
            assert_refused(&mut gate(), &bytes[..length]);
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert_refused(&mut gate(), &trailing);
    }

    #[test]
    fn non_configuring_states_reject_even_an_exact_reply() {
        let bytes = record(acknowledgement(), BINDING);
        for state in [State::Idle, State::Active(offer()), State::Ended] {
            let mut gate = Gate::new(state, BINDING);
            assert_refused(&mut gate, &bytes);
            assert!(!gate.admitted());
        }
    }

    #[test]
    fn two_callbacks_in_one_turn_cannot_send_twice() {
        let mut gate = gate();
        let bytes = record(acknowledgement(), BINDING);
        gate.admit(&bytes, || Ok(())).unwrap();
        assert_refused(&mut gate, &bytes);
        // The first send happened; do not falsify that fact. failed() instructs
        // the session to retire the epoch instead of accepting any more work.
        assert!(gate.admitted());
        assert!(gate.failed());
    }

    #[test]
    fn send_refusal_is_retained_even_when_output_swallows_it() {
        let mut gate = gate();
        let bytes = record(acknowledgement(), BINDING);
        let _ = gate.admit(&bytes, || Err(OutputRefused));
        assert!(!gate.admitted());
        assert!(gate.failed());
        assert_refused(&mut gate, &bytes);
    }

    #[test]
    fn invalid_then_valid_callback_cannot_hide_a_failed_turn() {
        let mut gate = gate();
        let _ = gate.admit(&[], || panic!("invalid reply reached transport"));
        assert_refused(&mut gate, &record(acknowledgement(), BINDING));
        assert!(!gate.admitted());
    }
}
