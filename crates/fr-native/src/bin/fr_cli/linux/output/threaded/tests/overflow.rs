//! Real bounded inbox/worker ownership; the deliberately blocked consumer is a
//! fixture, not a `PulseAudio` device or a network performance measurement.
use super::*;

#[test]
fn overflow_retains_exact_original_records_and_the_worker_accepts_more_after_draining() {
    let (drain, wait) = mpsc::sync_channel(1);
    let (drained, records) = mpsc::sync_channel(1);
    let (again, resumed) = mpsc::sync_channel(1);
    let (finish, done) = mpsc::sync_channel(1);
    let (mut output, shared, slot) = fixture(move |_, inbox, _| {
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        let first: Vec<_> = (0..PACKETS).map(|_| inbox.try_recv().unwrap()).collect();
        assert!(matches!(inbox.try_recv(), Err(TryRecvError::Empty)));
        drained.send(first).unwrap();
        resumed.recv_timeout(Duration::from_secs(2)).unwrap();
        let last = inbox.try_recv().unwrap();
        assert_eq!(&last.bytes[..last.len], &packet(10));
        assert!(matches!(inbox.try_recv(), Err(TryRecvError::Empty)));
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        true
    });
    output.job.as_mut().unwrap().acknowledged = true;
    let first_arrival = now(output.origin);
    for seq in 0..10 {
        output.receive(&packet(seq), &mut || true).unwrap();
    }
    let last_arrival = now(output.origin);
    assert_eq!(output.report.borrow().dropped_packets, 2);
    assert_eq!(shared.fault.load(Ordering::Acquire), 0);
    assert!(shared.live_at(last_arrival.0));
    assert!(slot.load(Ordering::Acquire));
    drain.send(()).unwrap();
    let originals = records.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(originals.len(), PACKETS);
    for (index, original) in originals.iter().enumerate() {
        assert_eq!(
            &original.bytes[..original.len],
            &packet(u64::try_from(index).unwrap())
        );
        assert!(original.arrived >= first_arrival && original.arrived <= last_arrival);
        assert_eq!(original.bytes.len(), RECORD_BYTES);
    }
    output.receive(&packet(10), &mut || true).unwrap();
    again.send(()).unwrap();
    assert_eq!(output.report.borrow().dropped_packets, 2);
    assert_eq!(
        output.report.borrow().submitted,
        0,
        "inbox admission is not PCM submission"
    );
    finish.send(()).unwrap();
    retired(&shared);
}

#[test]
fn overflow_counts_saturate_without_fabricating_failure_submission_or_recovery() {
    let (release, blocked) = mpsc::sync_channel(1);
    let (mut output, shared, _) = fixture(move |_, _, _| {
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        true
    });
    output.job.as_mut().unwrap().acknowledged = true;
    {
        let mut report = output.report.borrow_mut();
        report.dropped_packets = u64::MAX - 1;
        report.acknowledged = true;
        report.submitted = 9;
    }
    for seq in 0..12 {
        output.receive(&packet(seq), &mut || true).unwrap();
    }
    output.account();
    {
        let report = output.report.borrow();
        assert_eq!(report.dropped_packets, u64::MAX);
        assert_eq!(report.submitted, 9);
        assert_eq!(report.failed_generations, 0);
        assert_eq!(report.expired_generations, 0);
        assert_eq!(report.recovered_generations, 0);
        assert_eq!(report.absence, None);
        assert!(report.active());
    }
    output.reset();
    assert_eq!(output.report.borrow().dropped_packets, u64::MAX);
    assert!(!output.report.borrow().active());
    release.send(()).unwrap();
    retired(&shared);
}

#[test]
fn disconnected_inbox_still_ends_the_original_worker_instead_of_counting_loss() {
    let (ready, disconnected) = mpsc::sync_channel(1);
    let (release, blocked) = mpsc::sync_channel(1);
    let (mut output, shared, _) = fixture(move |_, inbox, _| {
        drop(inbox);
        ready.send(()).unwrap();
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        true
    });
    disconnected.recv_timeout(Duration::from_secs(1)).unwrap();
    output.job.as_mut().unwrap().acknowledged = true;
    assert!(output.receive(&packet(0), &mut || true).is_err());
    assert_eq!(shared.fault.load(Ordering::Acquire), Fault::Native as u8);
    assert!(shared.stopped.load(Ordering::Acquire));
    assert_eq!(output.report.borrow().dropped_packets, 0);
    release.send(()).unwrap();
    retired(&shared);
}

#[test]
fn full_inbox_never_bypasses_epoch_binding_or_wire_validation() {
    for change in [0, 1, 2, 3] {
        let (release, blocked) = mpsc::sync_channel(1);
        let (mut output, shared, _) = fixture(move |_, _, _| {
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
            true
        });
        output.job.as_mut().unwrap().acknowledged = true;
        for seq in 0..u64::try_from(PACKETS).unwrap() {
            output.receive(&packet(seq), &mut || true).unwrap();
        }
        let mut wrong = packet(9);
        match change {
            0 => wrong[0] ^= 1,
            1 => wrong[16..20].copy_from_slice(&74_u32.to_be_bytes()),
            2 => output.current.as_mut().unwrap().1.generation = AudioGeneration::from_raw(8),
            _ => output.current.as_mut().unwrap().1.max_packet_bytes = 1,
        }
        assert!(output.receive(&wrong, &mut || true).is_err());
        assert!(shared.stopped.load(Ordering::Acquire));
        assert_eq!(output.report.borrow().dropped_packets, 0);
        release.send(()).unwrap();
        retired(&shared);
    }
}

#[test]
fn denied_permission_on_a_full_inbox_stops_before_the_drop_path() {
    let (release, blocked) = mpsc::sync_channel(1);
    let (mut output, shared, _) = fixture(move |_, _, _| {
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        true
    });
    output.job.as_mut().unwrap().acknowledged = true;
    for seq in 0..u64::try_from(PACKETS).unwrap() {
        output.receive(&packet(seq), &mut || true).unwrap();
    }
    let until = shared.until.load(Ordering::Acquire);
    assert!(output.receive(&packet(9), &mut || false).is_err());
    assert_eq!(shared.until.load(Ordering::Acquire), until);
    assert!(shared.stopped.load(Ordering::Acquire));
    assert_eq!(output.report.borrow().dropped_packets, 0);
    release.send(()).unwrap();
    retired(&shared);
}

fn encoded_burst(
    configuration: AudioConfiguration,
    config: fr_core::audio::AudioStreamConfig,
) -> (Vec<fr_media::audio::AudioAccessUnit>, Vec<Vec<u8>>) {
    use fr_media::audio::{AudioEncoder, AudioPcmFrame};
    use fr_native::opus::Encoder;
    let mut codec = Encoder::new();
    codec.configure(config).unwrap();
    let units: Vec<_> = (0..9)
        .map(|sequence| {
            let samples: Vec<_> = (0..480)
                .map(|i| (i16::try_from(i % 97).unwrap() - 48) * 250)
                .collect();
            let pcm = AudioPcmFrame::from_interleaved(
                configuration.generation,
                configuration.channels,
                sequence * 480,
                &samples,
            )
            .unwrap();
            codec.submit_pcm(&pcm).unwrap();
            codec.poll_packet().unwrap().unwrap()
        })
        .collect();
    let records: Vec<_> = units
        .iter()
        .map(|unit| {
            let mut record = vec![0; RECORD_BYTES];
            let n = wire::encode_packet(
                &wire::AudioPacket {
                    direction: unit.direction(),
                    generation: unit.generation(),
                    sequence: unit.sequence(),
                    timestamp_samples: unit.timestamp_samples(),
                    duration_samples: unit.duration_samples(),
                    payload: unit.payload(),
                },
                73,
                &mut record,
            )
            .unwrap();
            record.truncate(n);
            record
        })
        .collect();
    (units, records)
}

#[test]
fn one_overflow_in_a_ten_ms_burst_preserves_real_opus_history_and_bounded_concealment() {
    use fr_client::audio::playout::{PlayoutClock, PlayoutResult};
    use fr_core::audio::AudioStreamConfig;
    use fr_media::audio::AudioDecoder;
    use fr_native::opus::{Decoder, playout::OpusPlayout};
    let configuration = AudioConfiguration {
        channels: AudioChannels::Mono,
        frame_duration_ms: 10,
        max_decoded_samples: 480,
        ..offer()
    };
    let config = AudioStreamConfig::new(
        configuration.direction,
        configuration.generation,
        configuration.channels,
        configuration.frame_duration_ms,
        configuration.jitter_target_ms,
    )
    .unwrap();
    let (units, records) = encoded_burst(configuration, config);
    let (release, blocked) = mpsc::sync_channel(1);
    let (result, outcome) = mpsc::sync_channel(1);
    let (mut output, shared, _) = fixture(move |_, inbox, _| {
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        let clock = |us, samples| PlayoutClock {
            now: ClientInstant(us),
            output_samples: samples,
        };
        let mut receiver = OpusPlayout::new(73, configuration, clock(0, 0)).unwrap();
        let mut reference = Decoder::new();
        reference.configure(config).unwrap();
        let mut origin = 0;
        for index in 0..PACKETS {
            let record = inbox.try_recv().unwrap();
            if index == 0 {
                origin = record.arrived.0;
            }
            receiver
                .receive_record_at(
                    &record.bytes[..record.len],
                    record.arrived,
                    clock(record.arrived.0, 0),
                )
                .unwrap();
        }
        assert!(matches!(inbox.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(receiver.queued_packets(), PACKETS);
        let mut concealed = 0;
        for (index, unit) in units.iter().enumerate() {
            let expected = if index < PACKETS {
                reference.submit_packet(unit).unwrap();
                reference.poll_pcm().unwrap().unwrap()
            } else {
                reference.decode_plc(480).unwrap()
            };
            let due = (u64::try_from(index).unwrap() + 2) * 480;
            let rendered = receiver
                .render(
                    || Ok(clock(origin + due * 1000 / 48, due)),
                    |pcm, receipt| {
                        assert!(
                            pcm.samples() == expected.samples(),
                            "native history differs"
                        );
                        assert_eq!(receipt.sequence, u64::try_from(index).unwrap());
                        assert_eq!(receipt.concealed, index == PACKETS);
                        concealed += usize::from(receipt.concealed);
                        Ok(())
                    },
                )
                .unwrap();
            assert!(matches!(rendered, PlayoutResult::Submitted(_)));
        }
        assert_eq!(receiver.error(), None);
        assert_eq!(receiver.queued_packets(), 0);
        result.send(concealed).unwrap();
        true
    });
    output.current = Some((73, configuration));
    output.job.as_mut().unwrap().acknowledged = true;
    for record in records {
        output.receive(&record, &mut || true).unwrap();
    }
    assert_eq!(output.report.borrow().dropped_packets, 1);
    assert_eq!(shared.fault.load(Ordering::Acquire), 0);
    release.send(()).unwrap();
    assert_eq!(outcome.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
    retired(&shared);
}
