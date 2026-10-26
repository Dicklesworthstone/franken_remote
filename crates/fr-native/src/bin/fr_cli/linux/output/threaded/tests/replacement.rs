//! Real bounded thread/metadata custody, not native device or audibility proof.
use super::*;

fn next_offer() -> AudioConfiguration {
    let mut next = offer();
    next.generation = AudioGeneration::from_raw(8);
    next
}
fn finished(output: &Output) {
    let until = Instant::now() + Duration::from_secs(2);
    while !output.job.as_ref().unwrap().thread.is_finished() {
        assert!(Instant::now() < until, "released owner must finish");
        thread::sleep(TICK);
    }
}

#[test]
fn one_pending_offer_waits_for_cleanup_then_starts_without_another_reset() {
    let (release, blocked) = mpsc::sync_channel(1);
    let (mut output, original, slot) = fixture(move |shared, _, _| {
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        shared.submitted.store(9, Ordering::Release);
        shared.fail(Fault::Expired);
        true
    });
    let original_until = original.until.load(Ordering::Acquire);
    output.reset();
    output.configure(73, next_offer()).unwrap();
    let until = output.setup_until.unwrap();
    for _ in 0..8 {
        output
            .service(&mut || true, &mut |_| panic!("no ACK before retirement"))
            .unwrap();
        assert!(output.pending.is_some());
        assert_eq!(output.setup_until, Some(until));
        assert!(Arc::ptr_eq(&output.job.as_ref().unwrap().shared, &original));
    }
    assert_eq!(original.until.load(Ordering::Acquire), original_until);
    assert!(original.stopped.load(Ordering::Acquire));
    assert_eq!(output.report.borrow().resets, 1);
    output.volume.set_muted(true);

    release.send(()).unwrap();
    finished(&output);
    let (release_new, blocked_new) = mpsc::sync_channel(1);
    let mut expected = [0; wire::AUDIO_CONFIGURED_RECORD_BYTES];
    wire::encode_configured(
        &wire::AudioConfigured {
            direction: next_offer().direction,
            generation: next_offer().generation,
            accepted: true,
            actual_channels: next_offer().channels,
            actual_sample_rate: 48_000,
            actual_frame_duration_ms: 20,
        },
        73,
        &mut expected,
    )
    .unwrap();
    assert!(
        output
            .start_pending(|shared, config| {
                assert_eq!(config.offer, next_offer());
                assert_eq!(config.setup_until, until);
                assert!(
                    config.volume.is_muted(),
                    "latest local mute reaches replacement"
                );
                Job::spawn(&slot, shared, move |shared, _, replies| {
                    while shared.until.load(Ordering::Acquire) == 0 {
                        assert!(Instant::now() < until);
                        thread::park_timeout(TICK);
                    }
                    assert!(forward_ack(shared, config.origin, until, &replies, &expected).is_ok());
                    shared.submitted.store(4, Ordering::Release);
                    blocked_new.recv_timeout(Duration::from_secs(2)).unwrap();
                    true
                })
            })
            .unwrap()
    );
    let replacement = output.job.as_ref().unwrap().shared.clone();
    assert!(!Arc::ptr_eq(&replacement, &original));
    assert_eq!(output.report.borrow().generation_start, 9);
    assert!(!output.report.borrow().active());
    while replacement.ack.load(Ordering::Acquire) != ACK_ACCEPTED {
        output
            .service(&mut || true, &mut |record| {
                assert_eq!(record, &expected);
                Ok(())
            })
            .unwrap();
        assert!(Instant::now() < until);
        thread::sleep(TICK);
    }
    output.configuration_admitted(73, next_offer()).unwrap();
    while replacement.submitted.load(Ordering::Acquire) != 4 {
        assert!(Instant::now() < until);
        thread::sleep(TICK);
    }
    output.account();
    {
        let report = output.report.borrow();
        assert_eq!(report.submitted, 13);
        assert_eq!(report.expired_generations, 1);
        assert_eq!(report.failed_generations, 0);
        assert_eq!(report.recovered_generations, 1);
        assert_eq!(report.resets, 1, "cleanup wait is not another failed epoch");
        assert!(report.active());
    }
    release_new.send(()).unwrap();
    retired(&replacement);
}

#[test]
fn pending_offer_cannot_be_replaced_or_replayed_by_another_configuration() {
    let (release, blocked) = mpsc::sync_channel(1);
    let (mut output, shared, _) = fixture(move |_, _, _| {
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        true
    });
    output.reset();
    assert!(
        output.configure(73, offer()).is_err(),
        "spent generation refuses"
    );
    output.configure(73, next_offer()).unwrap();
    let until = output.setup_until;
    let mut newer = next_offer();
    newer.generation = AudioGeneration::from_raw(9);
    for candidate in [offer(), next_offer(), newer] {
        assert!(output.configure(73, candidate).is_err());
        assert_eq!(output.current, Some((73, next_offer())));
        assert_eq!(output.setup_until, until);
    }
    output.reset();
    assert!(
        output.configure(73, next_offer()).is_err(),
        "cancelled setup stays spent"
    );
    output.configure(73, newer).unwrap();
    assert_eq!(output.current, Some((73, newer)));
    release.send(()).unwrap();
    retired(&shared);
}

#[test]
fn setup_expiry_discards_pending_metadata_without_spawning_or_renewing_old_work() {
    let (release, blocked) = mpsc::sync_channel(1);
    let (mut output, shared, _) = fixture(move |_, _, _| {
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        true
    });
    output.reset();
    output.configure(73, next_offer()).unwrap();
    let permission = shared.until.load(Ordering::Acquire);
    output.setup_until = Some(Instant::now());
    assert!(
        output
            .start_pending(|_, _| panic!("expired setup must not spawn"))
            .is_err()
    );
    assert!(output.pending.is_none());
    assert_eq!(shared.until.load(Ordering::Acquire), permission);
    assert_eq!(output.report.borrow().failed_generations, 1);
    assert_eq!(
        output.report.borrow().absence,
        Some("audio_output_setup_timeout")
    );
    output.account();
    assert_eq!(output.report.borrow().failed_generations, 1);
    release.send(()).unwrap();
    retired(&shared);
    assert!(
        output
            .start_pending(|_, _| panic!("late cleanup cannot revive setup"))
            .unwrap()
    );
}

#[test]
fn observation_loss_and_host_stop_cancel_waiting_setup_without_new_permission() {
    for host_stop in [false, true] {
        let (release, blocked) = mpsc::sync_channel(1);
        let (mut output, shared, _) = fixture(move |_, _, _| {
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
            true
        });
        output.reset();
        output.configure(73, next_offer()).unwrap();
        let permission = shared.until.load(Ordering::Acquire);
        if host_stop {
            let mut record = [0; wire::AUDIO_STOP_RECORD_BYTES];
            wire::encode_stop(
                &wire::AudioStop {
                    direction: next_offer().direction,
                    generation: next_offer().generation,
                    reason: AudioStopReason::UserMute,
                },
                73,
                &mut record,
            )
            .unwrap();
            output
                .receive(&record, &mut || panic!("stop requires no permission"))
                .unwrap();
            assert!(output.ended);
        } else {
            assert!(
                output
                    .service(&mut || false, &mut |_| panic!("no late ACK"))
                    .is_err()
            );
        }
        assert!(output.pending.is_none());
        assert!(output.setup_until.is_none());
        assert_eq!(shared.until.load(Ordering::Acquire), permission);
        assert!(shared.stopped.load(Ordering::Acquire));
        release.send(()).unwrap();
        retired(&shared);
        assert!(
            output
                .start_pending(|_, _| panic!("stopped pending setup cannot spawn"))
                .unwrap()
        );
    }
}

#[test]
fn failed_cleanup_never_releases_replacement_custody() {
    let (release, blocked) = mpsc::sync_channel(1);
    let (mut output, shared, slot) = fixture(move |_, _, _| {
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        false
    });
    output.reset();
    output.configure(73, next_offer()).unwrap();
    release.send(()).unwrap();
    finished(&output);
    assert!(!shared.retired.load(Ordering::Acquire));
    assert!(slot.load(Ordering::Acquire));
    assert!(
        !output
            .start_pending(|_, _| panic!("unproved cleanup cannot spawn"))
            .unwrap()
    );
    output.account();
    assert_eq!(output.report.borrow().failed_generations, 1);
    output.setup_until = Some(Instant::now());
    assert!(
        output
            .start_pending(|_, _| panic!("timeout cannot release custody"))
            .is_err()
    );
    assert_eq!(output.report.borrow().failed_generations, 2);
    assert!(slot.load(Ordering::Acquire));
}

#[test]
fn setup_deadline_fences_both_ack_custody_and_late_transport_admission() {
    let origin = Instant::now();
    let shared = Shared::new(0);
    shared.authorize_at(0).unwrap();
    let (replies, records) = mpsc::sync_channel(1);
    assert!(forward_ack(&shared, origin, origin, &replies, &ack_record()).is_err());
    assert!(
        records.try_recv().is_err(),
        "expired setup never emits an ACK"
    );
    assert_eq!(shared.fault.load(Ordering::Acquire), Fault::Setup as u8);

    let (release, blocked) = mpsc::sync_channel(1);
    let (mut output, shared, _) = fixture(move |_, _, _| {
        blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        true
    });
    output.job.as_mut().unwrap().acknowledged = true;
    output.setup_until = Some(Instant::now());
    assert!(output.configuration_admitted(73, offer()).is_err());
    assert!(!output.report.borrow().acknowledged);
    assert_eq!(shared.fault.load(Ordering::Acquire), Fault::Setup as u8);
    release.send(()).unwrap();
    retired(&shared);
}
