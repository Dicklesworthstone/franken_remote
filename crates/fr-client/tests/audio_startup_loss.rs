//! Original device-clock startup under loss. The codec is an explicit one-result
//! fixture; native Opus composition is exercised separately, not inferred here.
use fr_client::{
    audio::playout::{
        AudioPlayout, AudioSubmission, PlayoutClock, PlayoutError, PlayoutResult,
        decoder::PolledDecoder,
    },
    input::ClientInstant,
};
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::audio::{AudioAccessUnit, AudioMediaError, AudioPcmFrame};
use std::{cell::Cell, rc::Rc};

#[derive(Default)]
struct Counts {
    decoded: Cell<u64>,
    concealed: Cell<u64>,
    dropped: Cell<u64>,
}
struct Codec {
    counts: Rc<Counts>,
    config: Option<AudioStreamConfig>,
    result: Option<AudioPcmFrame>,
    next: u64,
}
impl Codec {
    fn frame(&self, at: u64) -> AudioPcmFrame {
        let c = self.config.unwrap();
        AudioPcmFrame::from_interleaved(
            c.generation(),
            c.channels(),
            at,
            &vec![
                1200;
                c.expected_samples_per_frame() as usize * usize::from(c.channels().count())
            ],
        )
        .unwrap()
    }
}
impl PolledDecoder for Codec {
    fn configure(&mut self, c: AudioStreamConfig) -> Result<(), AudioMediaError> {
        self.config = Some(c);
        Ok(())
    }
    fn poll_configured(&mut self) -> Result<bool, AudioMediaError> {
        Ok(true)
    }
    fn submit_packet(&mut self, p: &AudioAccessUnit) -> Result<(), AudioMediaError> {
        assert!(self.result.is_none());
        self.counts.decoded.set(self.counts.decoded.get() + 1);
        self.result = Some(self.frame(p.timestamp_samples()));
        self.next = p.timestamp_samples() + u64::from(p.duration_samples());
        Ok(())
    }
    fn submit_plc(&mut self, samples: u16) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        assert!(self.result.is_none());
        self.counts.concealed.set(self.counts.concealed.get() + 1);
        let frame = self.frame(self.next);
        self.next += u64::from(samples);
        Ok(Some(frame))
    }
    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        Ok(self.result.take())
    }
}
impl Drop for Codec {
    fn drop(&mut self) {
        self.counts.dropped.set(self.counts.dropped.get() + 1);
    }
}
fn clock(ms: u64) -> PlayoutClock {
    PlayoutClock {
        now: ClientInstant(1_000_000 + ms * 1000),
        output_samples: 50_000 + ms * 48,
    }
}
fn config(duration: u16, target: u16) -> AudioStreamConfig {
    AudioStreamConfig::new(
        AudioDirection::Downlink,
        AudioGeneration::from_raw(8),
        AudioChannels::Stereo,
        duration,
        target,
    )
    .unwrap()
}
fn owner(duration: u16, target: u16) -> (AudioPlayout<Codec>, Rc<Counts>) {
    let counts = Rc::new(Counts::default());
    let codec = Codec {
        counts: counts.clone(),
        config: None,
        result: None,
        next: 0,
    };
    (
        AudioPlayout::new(config(duration, target), codec, clock(0)).unwrap(),
        counts,
    )
}
fn packet(duration: u16, sequence: u64) -> AudioAccessUnit {
    AudioAccessUnit::new(
        AudioDirection::Downlink,
        AudioGeneration::from_raw(8),
        sequence,
        900_000 + sequence * u64::from(duration) * 48,
        duration * 48,
        false,
        &[0x42],
    )
    .unwrap()
}
fn render(owner: &mut AudioPlayout<Codec>, ms: u64) -> Result<PlayoutResult, PlayoutError> {
    owner.render(
        || Ok(clock(ms)),
        |pcm, receipt| {
            assert_eq!(pcm.timestamp_samples(), receipt.source_samples);
            assert!(receipt.output_samples <= clock(ms).output_samples);
            assert!(clock(ms).output_samples < receipt.output_valid_before);
            Ok(())
        },
    )
}
fn submitted(result: Result<PlayoutResult, PlayoutError>) -> AudioSubmission {
    let PlayoutResult::Submitted(r) = result.unwrap() else {
        panic!("due buffered audio must play")
    };
    r
}

#[test]
fn one_packet_starts_at_the_original_device_deadline_without_counting_missing_packets() {
    for duration in [5, 10, 20] {
        for target in [20, 40, 60] {
            let (mut owner, counts) = owner(duration, target);
            assert!(owner.receive(packet(duration, 0), clock(0)).unwrap());
            for ms in 0..u64::from(target) {
                assert_eq!(render(&mut owner, ms), Ok(PlayoutResult::Waiting));
                assert_eq!(
                    counts.decoded.get(),
                    0,
                    "the full reorder interval remains available"
                );
                assert_eq!(counts.concealed.get(), 0);
            }
            let first = submitted(render(&mut owner, u64::from(target)));
            assert_eq!(first.sequence, 0);
            assert!(!first.concealed);
            assert_eq!(
                first.output_samples,
                clock(u64::from(target)).output_samples
            );
            assert_eq!(first.valid_until, ClientInstant(1_100_000));
            assert_eq!(counts.decoded.get(), 1);
            assert_eq!(owner.queued_packets(), 0);
        }
    }
}

#[test]
fn lost_initial_packets_use_the_existing_finite_plc_timeline_then_resume_real_audio() {
    for duration in [10, 20] {
        let target = 3 * duration;
        let (mut owner, counts) = owner(duration, target);
        owner.receive(packet(duration, 0), clock(0)).unwrap();
        let first = submitted(render(&mut owner, u64::from(target)));
        assert_eq!(first.sequence, 0);
        for sequence in 1..=2 {
            let due = u64::from(target) + sequence * u64::from(duration);
            assert_eq!(render(&mut owner, due - 1), Ok(PlayoutResult::Waiting));
            let missing = submitted(render(&mut owner, due));
            assert_eq!(missing.sequence, sequence);
            assert!(missing.concealed);
            assert_eq!(
                missing.source_samples,
                900_000 + sequence * u64::from(duration) * 48
            );
        }
        let due = u64::from(target + 3 * duration);
        owner.receive(packet(duration, 3), clock(due - 1)).unwrap();
        let recovered = submitted(render(&mut owner, due));
        assert_eq!(recovered.sequence, 3);
        assert!(!recovered.concealed);
        assert_eq!(counts.decoded.get(), 2);
        assert_eq!(counts.concealed.get(), 2);
        assert_eq!(
            counts.dropped.get(),
            0,
            "no generation reset for initial packet loss"
        );
    }
}

#[test]
fn a_reordered_earlier_packet_is_still_selected_before_the_first_decode() {
    let (mut owner, counts) = owner(10, 40);
    owner.receive(packet(10, 8), clock(0)).unwrap();
    assert_eq!(render(&mut owner, 39), Ok(PlayoutResult::Waiting));
    owner.receive(packet(10, 7), clock(39)).unwrap();
    assert_eq!(counts.decoded.get(), 0);
    let first = submitted(render(&mut owner, 40));
    assert_eq!(first.sequence, 7);
    assert_eq!(first.valid_until, ClientInstant(1_139_000));
    assert_eq!(
        counts.concealed.get(),
        0,
        "no invented history before the first real packet"
    );
    assert_eq!(submitted(render(&mut owner, 50)).sequence, 8);
}

#[test]
fn empty_startup_is_silent_and_the_first_real_sequence_is_not_invented() {
    let (mut owner, counts) = owner(10, 40);
    for ms in [0, 40, 500, 10_000] {
        assert_eq!(render(&mut owner, ms), Ok(PlayoutResult::Waiting));
    }
    owner.receive(packet(10, 27), clock(10_000)).unwrap();
    assert_eq!(render(&mut owner, 10_039), Ok(PlayoutResult::Waiting));
    let first = submitted(render(&mut owner, 10_040));
    assert_eq!(first.sequence, 27);
    assert_eq!(counts.concealed.get(), 0);
}

#[test]
fn timed_startup_does_not_extend_expiry_or_allow_a_missed_slot() {
    for expired in [false, true] {
        let (mut owner, counts) = owner(10, 40);
        owner.receive(packet(10, 0), clock(0)).unwrap();
        let at = if expired {
            PlayoutClock {
                now: ClientInstant(1_100_000),
                output_samples: clock(40).output_samples,
            }
        } else {
            clock(50)
        };
        assert_eq!(
            owner.render(|| Ok(at), |_, _| panic!("not playable")),
            Err(if expired {
                PlayoutError::Expired
            } else {
                PlayoutError::MissedDeviceSlot
            })
        );
        assert_eq!(counts.decoded.get(), 0);
        assert_eq!(counts.dropped.get(), 1);
        assert_eq!(owner.queued_packets(), 0);
    }
}

#[test]
fn permission_loss_at_startup_retires_before_any_decode_or_submission() {
    let (mut owner, counts) = owner(10, 40);
    owner.receive(packet(10, 0), clock(0)).unwrap();
    assert_eq!(
        owner.render(|| Err(PlayoutError::Denied), |_, _| panic!("denied")),
        Err(PlayoutError::Denied)
    );
    assert_eq!(counts.decoded.get(), 0);
    assert_eq!(counts.dropped.get(), 1);
}

#[test]
fn unclocked_jitter_still_requires_its_packet_target() {
    use fr_client::audio::{AudioJitterBuffer, JitterDrainResult};
    let mut jitter = AudioJitterBuffer::with_config(config(10, 40)).unwrap();
    jitter.try_push_packet(packet(10, 0)).unwrap();
    for _ in 0..10 {
        assert_eq!(jitter.drain_checked().unwrap(), JitterDrainResult::Underrun);
        assert_eq!(jitter.queued_packet_count(), 1);
    }
    assert!(
        matches!(jitter.drain_for_playout().unwrap(), JitterDrainResult::Packet(p) if p.sequence() == 0)
    );
    jitter.stop();
    assert!(jitter.drain_for_playout().is_err());
}

#[test]
fn timed_start_does_not_create_unlimited_concealment_when_the_source_disappears() {
    let (mut owner, counts) = owner(10, 40);
    owner.receive(packet(10, 0), clock(0)).unwrap();
    assert!(!submitted(render(&mut owner, 40)).concealed);
    for ms in (50..=140).step_by(10) {
        assert!(submitted(render(&mut owner, ms)).concealed);
    }
    assert_eq!(counts.concealed.get(), 10);
    assert_eq!(render(&mut owner, 150), Err(PlayoutError::Expired));
    assert_eq!(counts.dropped.get(), 1);
    assert_eq!(owner.queued_packets(), 0);
}
