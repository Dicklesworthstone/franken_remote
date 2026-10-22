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
        volume: AudioVolumeControl::new(),
    };
    (output, shared, slot)
}
fn retired(shared: &Shared) {
    let until = Instant::now() + Duration::from_secs(2);
    while !shared.retired.load(Ordering::Acquire) {
        assert!(
            Instant::now() < until,
            "released fixture worker must retire"
        );
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
    assert_eq!(
        shared.fault.load(Ordering::Acquire),
        Fault::Permission as u8
    );
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
    output
        .receive(&packet(u64::try_from(PACKETS).unwrap()), &mut || true)
        .unwrap();
    assert_eq!(output.report.borrow().dropped_packets, 1);
    assert_eq!(shared.fault.load(Ordering::Acquire), 0);
    assert!(!shared.stopped.load(Ordering::Acquire));
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
    // The worker keeps playing after its acknowledgement, as in production,
    // until the test releases it.
    let (release, released) = mpsc::sync_channel::<()>(1);
    let (mut output, shared, _) = fixture(move |shared, _, replies| {
        let accepted = forward_ack(shared, origin, &replies, &ack_record()).is_ok();
        result.send(accepted).unwrap();
        let _ = released.recv_timeout(Duration::from_secs(5));
        true
    });
    assert!(!output.report.borrow().acknowledged);
    assert_eq!(shared.ack.load(Ordering::Acquire), ACK_PENDING);
    let until = Instant::now() + Duration::from_secs(2);
    // The session takes custody of the worker's one reply...
    while shared.ack.load(Ordering::Acquire) != ACK_ACCEPTED {
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
    // ...but custody is not success (596da32): only the session's transport
    // admission of THIS configuration reports it acknowledged.
    assert!(!output.report.borrow().acknowledged);
    assert!(output.configuration_admitted(74, offer()).is_err());
    assert!(!output.report.borrow().acknowledged);
    output.configuration_admitted(73, offer()).unwrap();
    assert!(output.report.borrow().acknowledged);
    release.send(()).unwrap();
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

#[test]
fn a_failed_worker_is_counted_once_through_service_reset_and_cleanup() {
    let (release, blocked) = mpsc::sync_channel(1);
    let (mut output, shared, _) = fixture(move |_, _, _| {
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        true
    });
    output.report.borrow_mut().acknowledged = true;
    shared.submitted.store(7, Ordering::Release);
    shared.fail(Fault::Expired);
    for _ in 0..3 {
        output.account();
    }
    assert!(output.service(&mut || true, &mut |_| Ok(())).is_err());
    output.reset();
    output.account();
    let report = output.report.borrow();
    assert_eq!(report.submitted, 7, "committed PCM survives the error");
    assert_eq!(report.expired_generations, 1);
    assert_eq!(report.failed_generations, 0);
    assert_eq!(report.first_failure, None);
    assert!(!report.active());
    assert_eq!(report.recovered_generations, 0);
    assert_eq!(report.absence, Some("audio_output_late"));
    drop(report);
    release.send(()).unwrap();
    retired(&shared);
    output.account();
    assert_eq!(output.report.borrow().expired_generations, 1);
}

#[test]
fn only_the_current_healthy_worker_can_clear_a_previous_absence() {
    let (release, blocked) = mpsc::sync_channel(1);
    let (mut output, shared, _) = fixture(move |_, _, _| {
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        true
    });
    {
        let mut report = output.report.borrow_mut();
        report.failed_generation("audio_output_failed", false);
        report.submitted = 12;
        report.begin_generation();
        report.acknowledged = true;
    }
    output.account();
    assert!(!output.report.borrow().active());
    assert_eq!(output.report.borrow().recovered_generations, 0);
    shared.submitted.store(1, Ordering::Release);
    output.account();
    {
        let report = output.report.borrow();
        assert!(report.active());
        assert_eq!(report.recovered_generations, 1);
        assert_eq!(report.absence, None);
        assert_eq!(report.first_failure, Some("audio_output_failed"));
    }
    shared.fail(Fault::Native);
    output.account();
    output.current = None;
    // The final snapshot can include an earlier committed write. Cleanup must
    // count it without pretending this failed, retired generation recovered.
    shared.submitted.store(2, Ordering::Release);
    output.account();
    let report = output.report.borrow();
    assert_eq!(report.submitted, 14);
    assert_eq!(report.failed_generations, 2);
    assert_eq!(report.recovered_generations, 1);
    assert_eq!(report.absence, Some("audio_output_failed"));
    assert!(!report.active());
    drop(report);
    release.send(()).unwrap();
    retired(&shared);
}

#[test]
fn retirement_collects_final_pcm_and_faults_before_replacement() {
    for (fault, expired, failed, first_failure) in [
        (Fault::Expired, 1, 0, None),
        (Fault::Native, 0, 1, Some("audio_output_failed")),
    ] {
        let (release, blocked) = mpsc::sync_channel(1);
        let (mut output, shared, _) = fixture(move |shared, _, _| {
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
            // These are the final stores for PCM already submitted and a
            // fault already detected before the session fenced this worker.
            shared.submitted.store(9, Ordering::Release);
            shared.fail(fault);
            true
        });
        output.report.borrow_mut().acknowledged = true;
        shared.submitted.store(4, Ordering::Release);
        output.reset();
        assert!(!output.account_retired());
        {
            let report = output.report.borrow();
            assert_eq!(report.submitted, 4);
            assert_eq!(report.expired_generations, 0);
            assert_eq!(report.failed_generations, 0);
            assert_eq!(report.first_failure, None);
        }

        // Force completion between the earlier account and the successful
        // retirement guard, without relying on a scheduler race or native audio.
        release.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        while !output.job.as_ref().unwrap().thread.is_finished() {
            assert!(Instant::now() < until, "released fixture must finish");
            thread::sleep(TICK);
        }
        assert!(shared.retired.load(Ordering::Acquire));
        assert!(output.current.is_none());
        for _ in 0..2 {
            assert!(output.account_retired());
            let report = output.report.borrow();
            assert_eq!(report.submitted, 9);
            assert_eq!(report.expired_generations, expired);
            assert_eq!(report.failed_generations, failed);
            assert_eq!(report.first_failure, first_failure);
            assert_eq!(report.absence, Some(reason(fault as u8)));
            assert_eq!(report.recovered_generations, 0);
            assert!(!report.active());
        }
    }
}

mod overflow;
