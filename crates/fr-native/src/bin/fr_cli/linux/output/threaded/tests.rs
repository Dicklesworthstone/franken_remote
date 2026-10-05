//! The real bounded session/worker bridge with deliberately stalled native work.
//! These tests make no `PulseAudio`, codec, audibility or live-network claim.
use super::*;
use fr_core::audio::{AudioChannels, AudioStopReason};

fn offer() -> AudioConfiguration {
    AudioConfiguration {
        direction: AudioDirection::Downlink,
        generation: AudioGeneration::from_raw(7),
        channels: AudioChannels::Stereo,
        sample_rate: 48_000,
        frame_duration_ms: 20,
        max_packet_bytes: 1275,
        max_decoded_samples: 960,
        jitter_target_ms: 20,
    }
}
fn fixture(
    work: impl FnOnce(&Shared, Receiver<Packet>, SyncSender<Ack>) -> bool + Send + 'static,
) -> (Output, Arc<Shared>, Arc<AtomicBool>) {
    let origin = Instant::now();
    let shared = Arc::new(Shared::new(0));
    shared.authorize_at(0).unwrap();
    let slot = Arc::new(AtomicBool::new(false));
    let job = Job::spawn(&slot, shared.clone(), work).unwrap();
    let output = Output {
        server: PathBuf::from("/unused-fixture"),
        sink: None,
        image: Some(PathBuf::from("/unused-fixture")),
        origin,
        job: Some(job),
        current: Some((73, offer())),
        last: Some(offer().generation),
        ended: false,
        report: Rc::new(RefCell::new(Report::default())),
    };
    (output, shared, slot)
}
fn retired(shared: &Shared) {
    let until = Instant::now() + Duration::from_secs(2);
    while !shared.retired.load(Ordering::Acquire) {
        assert!(Instant::now() < until, "released fixture worker must retire");
        thread::sleep(TICK);
    }
}
fn packet(sequence: u64) -> Vec<u8> {
    let mut bytes = vec![0; RECORD_BYTES];
    let n = wire::encode_packet(
        &wire::AudioPacket {
            direction: AudioDirection::Downlink,
            generation: offer().generation,
            sequence,
            timestamp_samples: sequence * 960,
            duration_samples: 960,
            payload: &[0xf8, 0xff, 0xfe],
        },
        73,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}

#[test]
fn expired_permission_is_terminal_even_before_the_worker_notices() {
    let shared = Shared::new(0);
    shared.authorize_at(0).unwrap();
    assert!(shared.live_at(PERMISSION_US - 1));
    assert!(!shared.live_at(PERMISSION_US));
    assert!(shared.authorize_at(PERMISSION_US).is_err());
    shared.progress.store(PERMISSION_US, Ordering::Release);
    assert!(shared.authorize_at(PERMISSION_US + 1).is_err());
    assert_eq!(shared.fault.load(Ordering::Acquire), Fault::Permission as u8);
}

#[test]
fn session_turns_cannot_hide_a_stalled_foreign_worker() {
    let shared = Shared::new(0);
    for now in [0, 50_000, 100_000, 150_000, 200_000] {
        shared.authorize_at(now).unwrap();
    }
    assert!(shared.authorize_at(STALL_US).is_err());
    assert_eq!(shared.fault.load(Ordering::Acquire), Fault::Stalled as u8);
    assert!(!shared.live_at(STALL_US));
}

#[test]
fn stop_and_clock_overflow_never_create_another_permission_window() {
    let shared = Shared::new(0);
    shared.stop();
    assert!(shared.authorize_at(0).is_err());
    let shared = Shared::new(u64::MAX);
    assert!(shared.authorize_at(u64::MAX).is_err());
    assert!(!shared.live_at(u64::MAX));
}

#[test]
fn fixed_packet_storage_preserves_receipt_and_refuses_oversize_before_copy() {
    let packet = Packet::new(&[1, 2, 3], ClientInstant(17)).unwrap();
    assert_eq!(packet.arrived, ClientInstant(17));
    assert_eq!(&packet.bytes[..packet.len], &[1, 2, 3]);
    assert!(Packet::new(&[0; RECORD_BYTES + 1], ClientInstant(17)).is_err());
}

#[test]
fn full_inbox_and_drop_return_while_foreign_work_is_still_blocked() {
    let (entered, started) = mpsc::sync_channel(1);
    let (release, blocked) = mpsc::sync_channel(1);
    let (mut output, shared, slot) = fixture(move |_, _packets, _replies| {
        entered.send(()).unwrap();
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        true
    });
    started.recv_timeout(Duration::from_secs(1)).unwrap();
    output.job.as_mut().unwrap().acknowledged = true;
    for sequence in 0..PACKETS {
        output
            .receive(&packet(u64::try_from(sequence).unwrap()), &mut || true)
            .unwrap();
    }
    assert!(
        output
            .receive(&packet(u64::try_from(PACKETS).unwrap()), &mut || true)
            .is_err()
    );
    assert_eq!(
        shared.fault.load(Ordering::Acquire),
        Fault::Backpressure as u8
    );
    drop(output);
    assert!(shared.stopped.load(Ordering::Acquire));
    assert!(
        slot.load(Ordering::Acquire),
        "no replacement before actual cleanup"
    );
    // The API calls above have all returned although foreign work is STILL blocked.
    release.send(()).unwrap();
    retired(&shared);
}

#[test]
fn reset_cannot_spawn_a_replacement_for_an_unretired_native_owner() {
    let (release, blocked) = mpsc::sync_channel(1);
    let (mut output, shared, slot) = fixture(move |_, _, _| {
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        true
    });
    output.reset();
    let mut next = offer();
    next.generation = AudioGeneration::from_raw(8);
    assert!(output.configure(73, next).is_err());
    assert!(
        Job::spawn(&slot, Arc::new(Shared::new(0)), |_, _, _| {
            panic!("second worker must not start")
        })
        .is_err()
    );
    assert_eq!(output.report.borrow().resets, 1);
    release.send(()).unwrap();
    retired(&shared);
}

fn ack_record() -> Ack {
    let mut record = [0; wire::AUDIO_CONFIGURED_RECORD_BYTES];
    wire::encode_configured(
        &wire::AudioConfigured {
            direction: offer().direction,
            generation: offer().generation,
            accepted: true,
            actual_channels: offer().channels,
            actual_sample_rate: 48_000,
            actual_frame_duration_ms: 20,
        },
        73,
        &mut record,
    )
    .unwrap();
    record
}

#[test]
fn queued_ack_is_not_success_until_the_session_accepts_the_exact_bytes() {
    let origin = Instant::now();
    let (result, completion) = mpsc::sync_channel(1);
    let (mut output, shared, _) = fixture(move |shared, _, replies| {
        let accepted = forward_ack(shared, origin, &replies, &ack_record()).is_ok();
        result.send(accepted).unwrap();
        true
    });
    assert!(!output.report.borrow().acknowledged);
    assert_eq!(shared.ack.load(Ordering::Acquire), ACK_PENDING);
    let until = Instant::now() + Duration::from_secs(2);
    while !output.report.borrow().acknowledged {
        output
            .service(&mut || true, &mut |bytes| {
                assert_eq!(bytes, &ack_record());
                assert_eq!(shared.ack.load(Ordering::Acquire), ACK_PENDING);
                Ok(())
            })
            .unwrap();
        assert!(Instant::now() < until);
        thread::sleep(TICK);
    }
    assert!(completion.recv_timeout(Duration::from_secs(1)).unwrap());
    retired(&shared);
}

#[test]
fn failed_transport_acceptance_never_unblocks_playback_as_acknowledged() {
    let origin = Instant::now();
    let (result, completion) = mpsc::sync_channel(1);
    let (mut output, shared, _) = fixture(move |shared, _, replies| {
        result
            .send(forward_ack(shared, origin, &replies, &ack_record()).is_ok())
            .unwrap();
        true
    });
    let until = Instant::now() + Duration::from_secs(2);
    while output
        .service(&mut || true, &mut |_| Err(ViewerAudioRefused))
        .is_ok()
    {
        assert!(Instant::now() < until);
        thread::sleep(TICK);
    }
    assert!(!completion.recv_timeout(Duration::from_secs(1)).unwrap());
    assert_eq!(shared.ack.load(Ordering::Acquire), ACK_PENDING);
    assert!(!output.report.borrow().acknowledged);
    retired(&shared);
}

#[test]
fn matching_stop_bypasses_a_full_inbox_without_renewing_permission() {
    let (release, blocked) = mpsc::sync_channel(1);
    let (mut output, shared, _) = fixture(move |_, _, _| {
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        true
    });
    output.job.as_mut().unwrap().acknowledged = true;
    for sequence in 0..PACKETS {
        output
            .receive(&packet(u64::try_from(sequence).unwrap()), &mut || true)
            .unwrap();
    }
    let mut stop = [0; wire::AUDIO_STOP_RECORD_BYTES];
    wire::encode_stop(
        &wire::AudioStop {
            direction: offer().direction,
            generation: offer().generation,
            reason: AudioStopReason::SessionEnded,
        },
        73,
        &mut stop,
    )
    .unwrap();
    output
        .receive(&stop, &mut || panic!("stop needs no renewed permission"))
        .unwrap();
    assert!(shared.stopped.load(Ordering::Acquire));
    output
        .service(&mut || false, &mut |_| panic!("no late ACK after stop"))
        .unwrap();
    assert_eq!(shared.ack.load(Ordering::Acquire), ACK_PENDING);
    release.send(()).unwrap();
    retired(&shared);
}
