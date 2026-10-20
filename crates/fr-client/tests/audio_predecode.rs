//! Production playout with an explicitly delayed decoder, not an Opus/device
//! qualification. Decode completion and both local clocks are independent.
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
struct Trace {
    now: Rc<Cell<u64>>,
    delay: Rc<Cell<u64>>,
    packets: Rc<Cell<usize>>,
    plc: Rc<Cell<usize>>,
    polls: Rc<Cell<usize>>,
    dropped: Rc<Cell<usize>>,
}
struct Delayed {
    trace: Trace,
    config: Option<AudioStreamConfig>,
    pcm: Option<AudioPcmFrame>,
    ready_at: u64,
    next: u64,
}
impl Delayed {
    fn new(trace: Trace) -> Self {
        Self {
            trace,
            config: None,
            pcm: None,
            ready_at: 0,
            next: 0,
        }
    }
    fn start(&mut self, at: u64) {
        assert!(
            self.pcm.is_none(),
            "at most one native result, never a PCM FIFO"
        );
        let c = self.config.unwrap();
        self.pcm = Some(
            AudioPcmFrame::from_interleaved(
                c.generation(),
                c.channels(),
                at,
                &vec![
                    8000;
                    c.expected_samples_per_frame() as usize * usize::from(c.channels().count())
                ],
            )
            .unwrap(),
        );
        self.next = at + u64::from(c.expected_samples_per_frame());
        self.ready_at = self.trace.now.get() + self.trace.delay.get();
    }
}
impl PolledDecoder for Delayed {
    fn configure(&mut self, config: AudioStreamConfig) -> Result<(), AudioMediaError> {
        self.config = Some(config);
        Ok(())
    }
    fn poll_configured(&mut self) -> Result<bool, AudioMediaError> {
        Ok(true)
    }
    fn submit_packet(&mut self, packet: &AudioAccessUnit) -> Result<(), AudioMediaError> {
        self.trace.packets.set(self.trace.packets.get() + 1);
        self.start(packet.timestamp_samples());
        Ok(())
    }
    fn submit_plc(&mut self, _samples: u16) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        self.trace.plc.set(self.trace.plc.get() + 1);
        self.start(self.next);
        Ok(None)
    }
    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        self.trace.polls.set(self.trace.polls.get() + 1);
        Ok(if self.trace.now.get() >= self.ready_at {
            self.pcm.take()
        } else {
            None
        })
    }
}
impl Drop for Delayed {
    fn drop(&mut self) {
        self.trace.dropped.set(self.trace.dropped.get() + 1);
    }
}
fn clock(ms: u64) -> PlayoutClock {
    PlayoutClock {
        now: ClientInstant(ms * 1000),
        output_samples: ms * 48,
    }
}
struct Case {
    owner: AudioPlayout<Delayed>,
    trace: Trace,
    config: AudioStreamConfig,
}
impl Case {
    fn new(duration: u16, channels: AudioChannels) -> Self {
        let config = AudioStreamConfig::new(
            AudioDirection::Downlink,
            AudioGeneration::from_raw(7),
            channels,
            duration,
            20,
        )
        .unwrap();
        let trace = Trace::default();
        let owner = AudioPlayout::new(config, Delayed::new(trace.clone()), clock(0)).unwrap();
        Self {
            owner,
            trace,
            config,
        }
    }
    fn packet(&self, seq: u64) -> AudioAccessUnit {
        let samples = u16::try_from(self.config.expected_samples_per_frame()).unwrap();
        AudioAccessUnit::new(
            self.config.direction(),
            self.config.generation(),
            seq,
            900_000 + seq * u64::from(samples),
            samples,
            false,
            &[0x42],
        )
        .unwrap()
    }
    fn receive(&mut self, seq: u64, ms: u64) {
        assert!(self.owner.receive(self.packet(seq), clock(ms)).unwrap());
    }
    fn render(&mut self, ms: u64) -> Result<PlayoutResult, PlayoutError> {
        self.trace.now.set(ms * 1000);
        self.owner.render(
            || Ok(clock(ms)),
            |_, receipt| {
                assert!(
                    receipt.output_samples <= clock(ms).output_samples,
                    "no early sound"
                );
                assert!(clock(ms).output_samples < receipt.output_valid_before);
                Ok(())
            },
        )
    }
    fn first(&mut self) {
        self.receive(0, 0);
        self.receive(1, 0);
        let PlayoutResult::Submitted(receipt) = self.render(20).unwrap() else {
            panic!("first output")
        };
        assert_eq!(receipt.sequence, 0);
        assert_eq!(self.trace.packets.get(), 1);
    }
}

#[test]
fn slow_decode_overlaps_playback_instead_of_starting_at_the_next_device_slot() {
    for duration in [10, 20] {
        for channels in [AudioChannels::Mono, AudioChannels::Stereo] {
            let mut c = Case::new(duration, channels);
            c.first();
            let duration = u64::from(duration);
            // More than one frame of decoder latency: starting at due would
            // necessarily miss the slot. Starting one slot ahead can complete.
            c.trace.delay.set((duration + 2) * 1000);
            assert_eq!(c.render(21), Ok(PlayoutResult::Waiting));
            assert_eq!(c.trace.packets.get(), 2);
            assert_eq!(c.owner.queued_packets(), 0);
            let due = 20 + duration;
            assert_eq!(c.render(due), Ok(PlayoutResult::Waiting));
            let PlayoutResult::Submitted(receipt) = c.render(due + 3).unwrap() else {
                panic!("prefetched result is ready inside its original slot")
            };
            assert_eq!(receipt.sequence, 1);
            assert_eq!(receipt.output_samples, due * 48);
            assert_eq!(receipt.output_valid_before, (due + duration) * 48);
            assert_eq!(receipt.source_samples, 900_000 + duration * 48);
            assert_eq!(receipt.valid_until, ClientInstant(100_000));
            assert!(!receipt.concealed);
        }
    }
}

#[test]
fn predecode_keeps_one_result_with_the_decoder_and_never_polls_or_submits_it_early() {
    let mut c = Case::new(10, AudioChannels::Mono);
    for seq in 0..8 {
        c.receive(seq, 0);
    }
    assert!(matches!(c.render(20), Ok(PlayoutResult::Submitted(_))));
    let polls = c.trace.polls.get();
    for _ in 0..1000 {
        assert_eq!(c.render(20), Ok(PlayoutResult::Waiting));
        assert_eq!(c.trace.packets.get(), 2);
        assert_eq!(c.trace.polls.get(), polls);
        assert_eq!(c.owner.queued_packets(), 6);
    }
    let PlayoutResult::Submitted(receipt) = c.render(30).unwrap() else {
        panic!("due")
    };
    assert_eq!(receipt.sequence, 1);
    assert_eq!(c.trace.polls.get(), polls + 1);
}

#[test]
fn startup_retains_its_full_reorder_window_before_anchoring_the_decoder() {
    let mut c = Case::new(10, AudioChannels::Mono);
    c.receive(1, 0);
    c.receive(2, 0);
    for ms in 0..20 {
        assert_eq!(c.render(ms), Ok(PlayoutResult::Waiting));
        assert_eq!(c.trace.packets.get(), 0);
    }
    c.receive(0, 19);
    let PlayoutResult::Submitted(receipt) = c.render(20).unwrap() else {
        panic!("due")
    };
    assert_eq!(receipt.sequence, 0);
    assert_eq!(c.trace.plc.get(), 0);
}

#[test]
fn a_missing_packet_keeps_its_reorder_window_and_plc_never_runs_ahead() {
    for arrives in [false, true] {
        let mut c = Case::new(10, AudioChannels::Mono);
        c.receive(0, 0);
        c.receive(2, 0);
        assert!(matches!(c.render(20), Ok(PlayoutResult::Submitted(_))));
        for ms in 21..30 {
            assert_eq!(c.render(ms), Ok(PlayoutResult::Waiting));
            assert_eq!(c.trace.packets.get(), 1);
            assert_eq!(c.trace.plc.get(), 0);
        }
        if arrives {
            c.receive(1, 29);
            assert_eq!(c.render(29), Ok(PlayoutResult::Waiting));
            assert_eq!(c.trace.packets.get(), 2);
        }
        let PlayoutResult::Submitted(receipt) = c.render(30).unwrap() else {
            panic!("due")
        };
        assert_eq!(receipt.sequence, 1);
        assert_eq!(receipt.concealed, !arrives);
        assert_eq!(c.trace.plc.get(), usize::from(!arrives));
    }
}

#[test]
fn predecode_does_not_invent_concealment_for_a_trailing_loss() {
    let mut c = Case::new(20, AudioChannels::Mono);
    c.receive(0, 0);
    assert!(matches!(c.render(20), Ok(PlayoutResult::Submitted(_))));
    for ms in 21..40 {
        assert_eq!(c.render(ms), Ok(PlayoutResult::Waiting));
        assert_eq!(c.trace.plc.get(), 0);
    }
    assert!(matches!(c.render(40), Ok(PlayoutResult::Submitted(r)) if r.concealed));
    assert_eq!(c.trace.plc.get(), 1);
}

#[test]
fn a_stalled_predecode_does_not_retime_its_slot_or_start_a_second_operation() {
    let mut c = Case::new(10, AudioChannels::Mono);
    c.first();
    c.receive(2, 20);
    c.trace.delay.set(1_000_000);
    for ms in 21..40 {
        assert_eq!(c.render(ms), Ok(PlayoutResult::Waiting));
    }
    assert_eq!(c.trace.packets.get(), 2);
    assert_eq!(c.render(40), Err(PlayoutError::MissedDeviceSlot));
    assert_eq!(c.owner.queued_packets(), 0);
    assert_eq!(c.trace.dropped.get(), 1);
}

#[test]
fn prefetched_packet_keeps_its_original_arrival_deadline_through_duplicates_and_new_work() {
    let mut c = Case::new(10, AudioChannels::Mono);
    c.first();
    assert_eq!(c.render(20), Ok(PlayoutResult::Waiting));
    for ms in 21..100 {
        let stopped_device = PlayoutClock {
            now: ClientInstant(ms * 1000),
            output_samples: 960,
        };
        assert!(!c.owner.receive(c.packet(1), stopped_device).unwrap());
    }
    c.owner
        .receive(
            c.packet(2),
            PlayoutClock {
                now: ClientInstant(99_000),
                output_samples: 960,
            },
        )
        .unwrap();
    assert_eq!(
        c.owner.render(
            || Ok(PlayoutClock {
                now: ClientInstant(100_000),
                output_samples: 960
            }),
            |_, _| panic!("no output after the first receipt's deadline"),
        ),
        Err(PlayoutError::Expired)
    );
    assert_eq!(c.trace.dropped.get(), 1);
}

#[test]
fn permission_fences_predecode_and_is_rechecked_before_later_output() {
    for before_decode in [false, true] {
        let mut c = Case::new(10, AudioChannels::Mono);
        c.first();
        if !before_decode {
            assert_eq!(c.render(21), Ok(PlayoutResult::Waiting));
        }
        let mut checks = 0;
        c.trace.now.set(30_000);
        let result = c.owner.render(
            || {
                checks += 1;
                if before_decode || checks == 2 {
                    Err(PlayoutError::Denied)
                } else {
                    Ok(clock(30))
                }
            },
            |_, _| panic!("permission was revoked"),
        );
        assert_eq!(result, Err(PlayoutError::Denied));
        assert_eq!(c.trace.packets.get(), if before_decode { 1 } else { 2 });
        assert_eq!(c.trace.dropped.get(), 1);
        assert_eq!(c.owner.queued_packets(), 0);
    }
}

#[test]
fn volume_changes_are_applied_at_output_not_when_the_future_packet_is_decoded() {
    let mut c = Case::new(10, AudioChannels::Mono);
    c.first();
    assert_eq!(c.render(21), Ok(PlayoutResult::Waiting));
    c.owner.volume_mut().set_muted(true);
    c.trace.now.set(30_000);
    let result = c.owner.render(
        || Ok(clock(30)),
        |pcm, receipt| {
            assert_eq!(receipt.sequence, 1);
            assert!(pcm.samples().iter().all(|sample| *sample == 0));
            Ok(())
        },
    );
    assert!(matches!(result, Ok(PlayoutResult::Submitted(_))));
}

#[test]
fn retirement_discards_the_prefetched_reply_before_a_new_generation_starts() {
    let mut c = Case::new(10, AudioChannels::Mono);
    c.first();
    assert_eq!(c.render(21), Ok(PlayoutResult::Waiting));
    c.owner.stop();
    assert_eq!(c.trace.dropped.get(), 1);
    assert_eq!(c.owner.queued_packets(), 0);
    c.config = AudioStreamConfig::new(
        AudioDirection::Downlink,
        AudioGeneration::from_raw(8),
        AudioChannels::Mono,
        10,
        20,
    )
    .unwrap();
    c.owner
        .reconfigure(c.config, Delayed::new(c.trace.clone()), clock(21))
        .unwrap();
    c.receive(0, 21);
    c.receive(1, 21);
    assert_eq!(c.render(40), Ok(PlayoutResult::Waiting));
    assert_eq!(
        c.trace.packets.get(),
        2,
        "new startup still waits for its slot"
    );
    let PlayoutResult::Submitted(receipt) = c.render(41).unwrap() else {
        panic!("new output")
    };
    assert_eq!(receipt.generation, c.config.generation());
    assert_eq!(receipt.sequence, 0);
}

#[test]
fn either_local_clock_regression_fences_a_prefetched_reply() {
    for clock in [
        PlayoutClock {
            now: ClientInstant(20_000),
            output_samples: 1008,
        },
        PlayoutClock {
            now: ClientInstant(22_000),
            output_samples: 1007,
        },
    ] {
        let mut c = Case::new(10, AudioChannels::Mono);
        c.first();
        assert_eq!(c.render(21), Ok(PlayoutResult::Waiting));
        assert_eq!(
            c.owner
                .render(|| Ok(clock), |_, _| panic!("regressed clock")),
            Err(PlayoutError::ClockRegression)
        );
        assert_eq!(c.trace.dropped.get(), 1);
        assert_eq!(c.owner.queued_packets(), 0);
    }
}

#[path = "audio_predecode/volume.rs"]
mod volume;
