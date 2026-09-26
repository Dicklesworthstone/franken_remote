//! Deferred-codec timing tests. The delayed PCM probe is not an Opus codec.
use fr_client::{
    audio::playout::{
        AudioPlayout, PlayoutClock, PlayoutError, PlayoutResult, decoder::PolledDecoder,
    },
    input::ClientInstant,
};
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::audio::{AudioAccessUnit, AudioMediaError, AudioPcmFrame};
use std::{cell::Cell, rc::Rc};
#[derive(Clone, Default)]
struct Counters {
    decoded: Rc<Cell<usize>>,
    concealed: Rc<Cell<usize>>,
    dropped: Rc<Cell<usize>>,
}
struct Deferred {
    config: Option<AudioStreamConfig>,
    pcm: Option<AudioPcmFrame>,
    next: u64,
    counts: Counters,
    ready: Rc<Cell<bool>>,
    available: Rc<Cell<bool>>,
}
impl Deferred {
    fn frame(&self, at: u64) -> AudioPcmFrame {
        let config = self.config.unwrap();
        AudioPcmFrame::from_interleaved(
            config.generation(),
            config.channels(),
            at,
            &vec![8000; usize::try_from(config.expected_samples_per_frame()).unwrap()],
        )
        .unwrap()
    }
}
impl Drop for Deferred {
    fn drop(&mut self) {
        self.counts.dropped.set(self.counts.dropped.get() + 1);
    }
}
impl PolledDecoder for Deferred {
    fn configure(&mut self, config: AudioStreamConfig) -> Result<(), AudioMediaError> {
        self.config = Some(config);
        Ok(())
    }
    fn poll_configured(&mut self) -> Result<bool, AudioMediaError> {
        Ok(self.ready.get())
    }
    fn submit_packet(&mut self, packet: &AudioAccessUnit) -> Result<(), AudioMediaError> {
        assert!(self.pcm.is_none(), "one outstanding operation");
        self.counts.decoded.set(self.counts.decoded.get() + 1);
        self.pcm = Some(self.frame(packet.timestamp_samples()));
        self.next = packet.timestamp_samples() + u64::from(packet.duration_samples());
        Ok(())
    }
    fn submit_plc(&mut self, samples: u16) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        assert!(self.pcm.is_none());
        self.counts.concealed.set(self.counts.concealed.get() + 1);
        self.pcm = Some(self.frame(self.next));
        self.next += u64::from(samples);
        Ok(None)
    }
    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        Ok(if self.available.get() {
            self.pcm.take()
        } else {
            None
        })
    }
}
fn config(
    generation: u64,
    direction: AudioDirection,
    duration: u16,
    target: u16,
) -> AudioStreamConfig {
    AudioStreamConfig::new(
        direction,
        AudioGeneration::from_raw(generation),
        AudioChannels::Mono,
        duration,
        target,
    )
    .unwrap()
}
fn clock(micros: u64, samples: u64) -> PlayoutClock {
    PlayoutClock {
        now: ClientInstant(micros),
        output_samples: samples,
    }
}
fn packet(c: AudioStreamConfig, seq: u64) -> AudioAccessUnit {
    let count = u16::try_from(c.expected_samples_per_frame()).unwrap();
    AudioAccessUnit::new(
        c.direction(),
        c.generation(),
        seq,
        900_000 + seq * u64::from(count),
        count,
        false,
        &[0x42],
    )
    .unwrap()
}
struct Case {
    owner: AudioPlayout<Deferred>,
    counts: Counters,
    ready: Rc<Cell<bool>>,
    available: Rc<Cell<bool>>,
    config: AudioStreamConfig,
}
fn case() -> Case {
    let config = config(7, AudioDirection::Downlink, 10, 20);
    let counts = Counters::default();
    let ready = Rc::new(Cell::new(false));
    let available = Rc::new(Cell::new(false));
    let decoder = Deferred {
        config: None,
        pcm: None,
        next: 0,
        counts: counts.clone(),
        ready: ready.clone(),
        available: available.clone(),
    };
    Case {
        owner: AudioPlayout::new(config, decoder, clock(0, 0)).unwrap(),
        counts,
        ready,
        available,
        config,
    }
}
fn turn(case: &mut Case, at: PlayoutClock) -> Result<PlayoutResult, PlayoutError> {
    case.owner.render(|| Ok(at), |_, _| Ok(()))
}
fn queue(case: &mut Case) {
    for seq in [0, 2, 3] {
        case.owner
            .receive(packet(case.config, seq), clock(0, 0))
            .unwrap();
    }
}
#[test]
fn configuration_must_really_complete_before_decode_and_readiness() {
    let mut c = case();
    queue(&mut c);
    assert!(!c.owner.poll_configured(clock(0, 0)).unwrap());
    assert_eq!(turn(&mut c, clock(20_000, 960)), Ok(PlayoutResult::Waiting));
    assert_eq!(c.counts.decoded.get(), 0);
    c.ready.set(true);
    assert!(c.owner.poll_configured(clock(20_000, 960)).unwrap());
    assert_eq!(turn(&mut c, clock(20_000, 960)), Ok(PlayoutResult::Waiting));
    assert_eq!(c.counts.decoded.get(), 1);
}
#[test]
fn pending_packet_and_plc_are_submitted_once_and_keep_original_device_slots() {
    let mut c = case();
    c.ready.set(true);
    queue(&mut c);
    for step in 0..2 {
        let due = 960 + step * 480;
        c.available.set(false);
        for i in 0..10 {
            assert_eq!(
                turn(&mut c, clock(20_000 + step * 10_000 + i, due)),
                Ok(PlayoutResult::Waiting)
            );
        }
        assert_eq!(c.counts.decoded.get(), 1);
        assert_eq!(c.counts.concealed.get(), usize::try_from(step).unwrap());
        c.available.set(true);
        let PlayoutResult::Submitted(receipt) =
            turn(&mut c, clock(20_100 + step * 10_000, due + 5)).unwrap()
        else {
            panic!("completed frame")
        };
        assert_eq!(receipt.sequence, step);
        assert_eq!(receipt.output_samples, due);
        assert_eq!(receipt.output_valid_before, due + 480);
        assert_eq!(receipt.concealed, step == 1);
        assert_eq!(
            turn(&mut c, clock(20_100 + step * 10_000, due + 5)),
            Ok(PlayoutResult::Waiting)
        );
    }
}
#[test]
fn late_pcm_is_never_retimed_to_a_new_device_slot() {
    let mut c = case();
    c.ready.set(true);
    queue(&mut c);
    assert_eq!(turn(&mut c, clock(20_000, 960)), Ok(PlayoutResult::Waiting));
    c.available.set(true);
    assert_eq!(
        turn(&mut c, clock(30_000, 1440)),
        Err(PlayoutError::MissedDeviceSlot)
    );
    assert_eq!(c.counts.decoded.get(), 1);
    assert_eq!(c.counts.dropped.get(), 1);
}
#[test]
fn a_pending_decode_keeps_its_first_arrival_deadline_even_with_new_packets() {
    let mut c = case();
    c.ready.set(true);
    queue(&mut c);
    assert_eq!(turn(&mut c, clock(20_000, 960)), Ok(PlayoutResult::Waiting));
    c.owner
        .receive(packet(c.config, 4), clock(99_999, 960))
        .unwrap();
    c.available.set(true);
    assert_eq!(
        turn(&mut c, clock(100_000, 960)),
        Err(PlayoutError::Expired)
    );
    assert_eq!(c.counts.dropped.get(), 1);
}
#[test]
fn revoked_or_stopped_pending_decode_never_submits_pcm() {
    for explicit_stop in [false, true] {
        let mut c = case();
        c.ready.set(true);
        queue(&mut c);
        assert_eq!(turn(&mut c, clock(20_000, 960)), Ok(PlayoutResult::Waiting));
        c.available.set(true);
        if explicit_stop {
            c.owner.stop();
        }
        let result = c.owner.render(
            || {
                if explicit_stop {
                    Ok(clock(21_000, 1008))
                } else {
                    Err(PlayoutError::Denied)
                }
            },
            |_, _| panic!("retired decode cannot reach output"),
        );
        assert_eq!(
            result,
            Err(if explicit_stop {
                PlayoutError::Stopped
            } else {
                PlayoutError::Denied
            })
        );
        assert_eq!(c.counts.dropped.get(), 1);
    }
}
