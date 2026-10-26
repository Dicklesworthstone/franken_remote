use super::*;

fn healthy() -> Load {
    Load {
        retained_bytes: 1024,
        retained_pictures: 1,
        decoding: false,
        work_us: Some(1000),
    }
}
fn backlog() -> Load {
    Load {
        retained_bytes: 2048,
        retained_pictures: 2,
        ..healthy()
    }
}
fn measured(view: Binding, at_us: u64, until_us: u64, load: Load) -> ReceiverSample {
    ReceiverSample::Measured(SharedSample {
        view,
        at_us,
        until_us,
        load,
    })
}
fn pair(
    now: u64,
    send: Availability,
    load: Load,
) -> (Option<(u64, Availability)>, ReceiverEvidence) {
    (Some((now, send)), ReceiverEvidence::Measured(load))
}

#[test]
fn receiver_absence_is_unknown_and_live_samples_keep_the_original_deadline() {
    let view = binding();
    assert_eq!(
        ReceiverSample::default().current(view, 0),
        ReceiverEvidence::Unknown
    );
    assert_eq!(
        ReceiverSample::Unnegotiated(view).current(view, 0),
        ReceiverEvidence::Unobserved
    );
    let original = measured(view, 10_000, 150_000, healthy());
    assert_eq!(original.current(view, 9999), ReceiverEvidence::Unknown);
    for now in [10_000, 50_000, 149_999] {
        assert_eq!(
            original.current(view, now),
            ReceiverEvidence::Measured(healthy())
        );
        // Recopying a still-valid reply on a later service turn cannot grant
        // another sample lifetime from that new copy timestamp.
        let copied = measured(view, now, 150_000, healthy());
        assert_eq!(copied.current(view, 150_000), ReceiverEvidence::Unknown);
    }
    assert_eq!(original.current(view, 150_000), ReceiverEvidence::Unknown);
    assert_eq!(original.current(view, 150_001), ReceiverEvidence::Unknown);
    assert_eq!(
        measured(view, 150_000, 150_000, healthy()).current(view, 150_000),
        ReceiverEvidence::Unknown
    );
}

#[test]
fn every_binding_component_fences_measured_and_unnegotiated_receiver_evidence() {
    let view = binding();
    let mut different = [view; 9];
    different[0].parent.id += 1;
    different[1].parent.host_boot = fr_core::ids::HostBootId::from_raw(20);
    different[2].parent.os_session = fr_core::ids::OsSessionId::from_raw(30);
    different[3].parent.remote_session = fr_core::ids::RemoteSessionId::from_raw(40);
    different[4].display += 1;
    different[5].geometry = view.geometry.next().unwrap();
    different[6].configuration = view.configuration.next().unwrap();
    different[7].recovery = view.recovery.next().unwrap();
    different[8].viewport = view.viewport.next().unwrap();
    for replacement in different {
        for original in [
            measured(view, 10_000, 150_000, healthy()),
            ReceiverSample::Unnegotiated(view),
        ] {
            assert_eq!(
                original.current(replacement, 10_001),
                ReceiverEvidence::Unknown
            );
        }
    }
}

#[test]
fn ready_sender_and_healthy_decoder_on_different_viewers_never_form_a_probe_path() {
    let mut pacing = pacing();
    let slow_decoder = Load {
        decoding: true,
        work_us: Some(200_000),
        ..healthy()
    };
    for now in (10_000_u64..=2_500_000).step_by(10_000) {
        if now.is_multiple_of(100_000) {
            complete(&mut pacing, now, true);
        }
        let a = pair(now, Availability::Ready, slow_decoder);
        let b = pair(now, Availability::Blocked, healthy());
        for peers in [[a, b], [b, a]] {
            let evidence = peer_evidence(now, pacing.interval_us(), peers.into_iter());
            assert_eq!(evidence.send, Availability::Ready);
            assert_eq!(evidence.receiver, ReceiverEvidence::Unknown);
        }
        let evidence = peer_evidence(now, pacing.interval_us(), [a, b].into_iter());
        pacing.sample(now, true, evidence).unwrap();
    }
    assert_eq!(pacing.interval_us(), 100_000);
    assert!(
        !pacing
            .controller
            .decisions()
            .any(|r| r.reason == pacing::Reason::HeadroomProbe)
    );
}

#[test]
fn a_healthy_accepted_sibling_keeps_its_whole_load_and_is_independent_of_laggards() {
    let ready = pair(10_000, Availability::Ready, healthy());
    let slow = pair(10_000, Availability::Blocked, backlog());
    for peers in [[ready, slow], [slow, ready]] {
        assert_eq!(
            peer_evidence(10_001, 100_000, peers.into_iter()),
            PeerEvidence {
                send: Availability::Ready,
                receiver: ReceiverEvidence::Measured(healthy())
            }
        );
    }
    let other = Load {
        retained_bytes: 1,
        work_us: Some(20_000),
        ..healthy()
    };
    let second = pair(10_000, Availability::Ready, other);
    // Stable subscriber order chooses one real report, including the bytes and
    // work from that report; smaller components from its sibling are not merged.
    assert_eq!(
        peer_evidence(10_001, 100_000, [ready, second].into_iter()).receiver,
        ReceiverEvidence::Measured(healthy())
    );
    assert_eq!(
        peer_evidence(10_001, 100_000, [second, ready].into_iter()).receiver,
        ReceiverEvidence::Measured(other)
    );
}

#[test]
fn receiver_slowdown_requires_live_positive_pressure_from_every_eligible_viewer() {
    let decoder = Load {
        decoding: true,
        work_us: Some(200_000),
        ..healthy()
    };
    let first = pair(10_000, Availability::Ready, backlog());
    let second = pair(10_000, Availability::Ready, decoder);
    assert_eq!(
        peer_evidence(10_001, 100_000, [first, second].into_iter()).receiver,
        ReceiverEvidence::Measured(backlog())
    );
    assert_eq!(
        peer_evidence(10_001, 100_000, [second, first].into_iter()).receiver,
        ReceiverEvidence::Measured(decoder)
    );
    for missing in [ReceiverEvidence::Unknown, ReceiverEvidence::Unobserved] {
        assert_eq!(
            peer_evidence(10_001, 100_000, [first, (None, missing)].into_iter()).receiver,
            ReceiverEvidence::Unknown
        );
    }
    let view = binding();
    for missing in [
        measured(view, 20_000, 30_000, backlog()).current(view, 10_001),
        measured(view, 1000, 10_001, backlog()).current(view, 10_001),
    ] {
        assert_eq!(
            peer_evidence(10_001, 100_000, [first, (None, missing)].into_iter()).receiver,
            ReceiverEvidence::Unknown
        );
    }
    assert_eq!(
        peer_evidence(10_001, 100_000, [].into_iter()).receiver,
        ReceiverEvidence::Unknown
    );
}

#[test]
fn all_slow_receivers_reduce_capture_after_dwell_without_local_pressure() {
    let decoder = Load {
        decoding: true,
        work_us: Some(200_000),
        ..healthy()
    };
    for (load, reason) in [
        (backlog(), pacing::Reason::ReceiverBacklog),
        (decoder, pacing::Reason::DecoderWork),
    ] {
        let mut pacing = pacing();
        for now in (0..=150_000).step_by(10_000) {
            let slow = pair(now, Availability::Ready, load);
            let evidence = peer_evidence(now, pacing.interval_us(), [slow, slow].into_iter());
            pacing.sample(now, true, evidence).unwrap();
        }
        assert_eq!(pacing.interval_us(), 200_000);
        assert!(pacing.controller.decisions().any(|r| r.reason == reason));
        let report = pacing.controller.report().unwrap();
        assert_eq!(report.sample.capture_credit, Availability::Ready);
        assert_eq!(report.sample.send, Availability::Ready);
        assert_eq!(report.sample.source_work_us, None);
    }
}

#[test]
fn unknown_decoder_work_and_active_decoding_cannot_justify_headroom() {
    for load in [
        Load {
            work_us: None,
            ..healthy()
        },
        Load {
            decoding: true,
            ..healthy()
        },
        Load {
            work_us: Some(75_001),
            ..healthy()
        },
    ] {
        assert_eq!(
            peer_evidence(
                10_000,
                100_000,
                [pair(10_000, Availability::Ready, load)].into_iter()
            )
            .receiver,
            ReceiverEvidence::Unknown
        );
    }
    let boundary = Load {
        work_us: Some(75_000),
        ..healthy()
    };
    assert_eq!(
        peer_evidence(
            10_000,
            100_000,
            [pair(10_000, Availability::Ready, boundary)].into_iter()
        )
        .receiver,
        ReceiverEvidence::Measured(boundary)
    );
}

#[test]
fn only_an_explicit_unnegotiated_accepted_peer_preserves_local_only_probes() {
    let local = (
        Some((10_000, Availability::Ready)),
        ReceiverEvidence::Unobserved,
    );
    let unknown = (
        Some((10_000, Availability::Ready)),
        ReceiverEvidence::Unknown,
    );
    assert_eq!(
        peer_evidence(10_001, 100_000, [unknown, local].into_iter()).receiver,
        ReceiverEvidence::Unobserved
    );
    assert_eq!(
        peer_evidence(10_001, 100_000, [unknown, unknown].into_iter()).receiver,
        ReceiverEvidence::Unknown
    );
    let blocked = (
        Some((10_000, Availability::Blocked)),
        ReceiverEvidence::Unobserved,
    );
    assert_eq!(
        peer_evidence(
            10_001,
            100_000,
            [pair(10_000, Availability::Ready, backlog()), blocked].into_iter()
        )
        .receiver,
        ReceiverEvidence::Unknown
    );
}

#[test]
fn enqueue_evidence_spans_one_cadence_but_never_the_unbounded_silence_after_it() {
    for (interval, lifetime) in [(50_000, 100_000), (200_000, 200_000), (1_000_000, 250_000)] {
        for availability in [Availability::Ready, Availability::Blocked] {
            let sample = (Some((1000, availability)), ReceiverEvidence::Unobserved);
            assert_eq!(
                peer_evidence(1000 + lifetime, interval, [sample].into_iter()).send,
                availability
            );
            assert_eq!(
                peer_evidence(1001 + lifetime, interval, [sample].into_iter()).send,
                Availability::Unknown
            );
            assert_eq!(
                peer_evidence(999, interval, [sample].into_iter()).send,
                Availability::Unknown
            );
        }
    }
}

#[test]
fn recovered_receiver_allows_a_real_cadence_sender_to_probe_after_the_full_dwell() {
    let mut pacing = pacing();
    let view = binding();
    let mut last_capture = None;
    let mut send = None;
    let mut receiver = ReceiverSample::Unknown;
    let mut captures = 0;
    for now in (0_u64..=3_500_000).step_by(10_000) {
        // Real enqueues accompany captures, not maintenance ticks. A bounded
        // 10 ms late turn exposes the old fixed-100 ms evidence gap at 200 ms.
        let due = last_capture.map_or(100_000, |last| last + pacing.interval_us() + 10_000);
        if now >= due {
            complete(&mut pacing, now, true);
            last_capture = Some(now);
            send = Some((now, Availability::Ready));
            captures += 1;
        }
        if now.is_multiple_of(50_000) {
            receiver = measured(
                view,
                now,
                now + 150_000,
                if now < 400_000 { backlog() } else { healthy() },
            );
        }
        let evidence = peer_evidence(
            now,
            pacing.interval_us(),
            [(send, receiver.current(view, now))].into_iter(),
        );
        pacing.sample(now, true, evidence).unwrap();
        if now == 350_000 {
            assert_eq!(pacing.interval_us(), 200_000);
        }
    }
    assert!((10..30).contains(&captures));
    assert!(pacing.interval_us() < 200_000);
    assert!(
        pacing
            .controller
            .decisions()
            .any(|r| r.reason == pacing::Reason::ReceiverBacklog)
    );
    assert!(
        pacing
            .controller
            .decisions()
            .any(|r| r.reason == pacing::Reason::HeadroomProbe)
    );
}

#[test]
fn missing_receiver_samples_do_not_disable_real_capture_credit_or_source_pressure() {
    for source_pressure in [false, true] {
        let mut pacing = pacing();
        if source_pressure {
            pacing.start(0).unwrap();
        }
        for now in (0..=250_000).step_by(10_000) {
            pacing
                .sample(
                    now,
                    false,
                    PeerEvidence {
                        send: Availability::Ready,
                        receiver: ReceiverEvidence::Unknown,
                    },
                )
                .unwrap();
        }
        let expected = if source_pressure {
            pacing::Reason::SourceWork
        } else {
            pacing::Reason::CaptureCredit
        };
        assert_eq!(pacing.interval_us(), 200_000);
        assert!(pacing.controller.decisions().any(|r| r.reason == expected));
    }
}
