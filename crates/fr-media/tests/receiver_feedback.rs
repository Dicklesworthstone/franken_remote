use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::{
    pacing::{
        Availability, Controller, Observation, Policy, Reason, ReceiverEvidence as E, Sample,
    },
    receiver_feedback::*,
};
use fr_wire::{
    decoder::Binding,
    input::{InputDelivery::Reliable, InputDirection::*},
    negotiation::ControlBinding,
    receiver_metrics::{self as w, Load, Message},
};
fn binding() -> Binding {
    Binding {
        parent: ControlBinding {
            id: 1,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        },
        display: 4,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
fn load() -> Load {
    Load {
        retained_bytes: 500,
        retained_pictures: 1,
        decoding: false,
        work_us: Some(5000),
    }
}
fn reply(sequence: u64) -> Vec<u8> {
    let mut b = vec![0; w::REPLY_BYTES];
    w::encode(
        Message::Reply {
            sequence,
            load: load(),
        },
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut b,
        ViewerToHost,
        Reliable,
    )
    .unwrap();
    b
}
fn query(sequence: u64) -> Vec<u8> {
    let mut b = vec![0; w::QUERY_BYTES];
    w::encode(
        Message::Query { sequence },
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut b,
        HostToViewer,
        Reliable,
    )
    .unwrap();
    b
}
#[test]
fn old_reply_is_never_retimed_at_receipt_or_by_replay() {
    let mut h = Requester::new(binding(), ProtocolLimits::ABSOLUTE, 100).unwrap();
    h.prepare(100).unwrap();
    h.queued(200).unwrap();
    assert!(h.receive(&reply(1), 140_000).unwrap());
    assert_eq!(h.evidence(150_099).unwrap(), E::Measured(load()));
    assert!(!h.receive(&reply(1), 150_100).unwrap());
    assert_eq!(h.evidence(150_100).unwrap(), E::Unknown);
}
#[test]
fn query_backpressure_retains_original_bytes_sequence_and_deadline() {
    let mut h = Requester::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap();
    h.prepare(0).unwrap();
    let (bytes, until) = h.pending(0).unwrap().unwrap();
    let bytes = bytes.to_vec();
    for now in [1, 49_999, 100_000, 149_999] {
        h.prepare(now).unwrap();
        assert_eq!(h.pending(now).unwrap().unwrap(), (bytes.as_slice(), until));
    }
    assert!(h.pending(until).unwrap().is_none());
    assert_eq!(h.queued(until), Err(Error::Expired));
    h.prepare(until).unwrap();
    assert_ne!(h.pending(until).unwrap().unwrap().0, bytes);
    assert!(!h.receive(&reply(1), until).unwrap());
}
#[test]
fn future_unsent_foreign_and_expired_samples_cannot_supply_headroom() {
    let mut h = Requester::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap();
    assert_eq!(h.receive(&reply(1), 0), Err(Error::Sequence));
    h.prepare(0).unwrap();
    assert_eq!(h.receive(&reply(1), 0), Err(Error::Sequence));
    h.queued(0).unwrap();
    let mut b = reply(1);
    b[24] ^= 1;
    assert!(h.receive(&b, 1).is_err());
    assert!(!h.receive(&reply(1), SAMPLE_LIFETIME_US).unwrap());
    assert_eq!(h.evidence(SAMPLE_LIFETIME_US).unwrap(), E::Unknown);
}
#[test]
fn response_backpressure_and_bursts_do_not_replace_the_original_sample() {
    let mut r = Responder::new(binding(), ProtocolLimits::ABSOLUTE, 500_000).unwrap();
    assert!(r.receive(&query(1), load(), 500_000).unwrap());
    let (bytes, until) = r.pending(500_000).unwrap().unwrap();
    let bytes = bytes.to_vec();
    for (i, now) in [500_001, 530_000, 599_999].into_iter().enumerate() {
        assert!(
            !r.receive(
                &query(i as u64 + 2),
                Load {
                    work_us: Some(100_000),
                    ..load()
                },
                now
            )
            .unwrap()
        );
        assert_eq!(r.pending(now).unwrap().unwrap(), (bytes.as_slice(), until));
    }
    assert!(r.pending(until).unwrap().is_none());
    assert!(!r.receive(&query(4), load(), until).unwrap());
    assert!(r.receive(&query(5), load(), until).unwrap());
}
#[test]
fn independent_clocks_do_not_enter_the_host_validity_calculation() {
    let mut h = Requester::new(binding(), ProtocolLimits::ABSOLUTE, 100).unwrap();
    let mut v = Responder::new(binding(), ProtocolLimits::ABSOLUTE, 90_000_000).unwrap();
    h.prepare(100).unwrap();
    let bytes = h.pending(100).unwrap().unwrap().0.to_vec();
    h.queued(100).unwrap();
    v.receive(&bytes, load(), 90_000_000).unwrap();
    let bytes = v.pending(90_080_000).unwrap().unwrap().0.to_vec();
    v.queued(90_080_000).unwrap();
    assert!(h.receive(&bytes, 100_000).unwrap());
    assert_eq!(h.evidence(150_100).unwrap(), E::Unknown);
}
fn sample(now_us: u64) -> Sample {
    Sample {
        now_us,
        source_work_us: Some(5000),
        send: Availability::Ready,
        capture_credit: Availability::Ready,
        observation: Some(Observation {
            at_us: now_us,
            changed: true,
        }),
    }
}
fn controller() -> Controller {
    Controller::new(Policy {
        minimum_interval_us: 20_000,
        maximum_interval_us: 200_000,
    })
    .unwrap()
}
#[test]
fn actual_receiver_backlog_and_decoder_work_have_separate_reduction_reasons() {
    for (measurement, reason) in [
        (
            Load {
                retained_pictures: 3,
                ..load()
            },
            Reason::ReceiverBacklog,
        ),
        (
            Load {
                work_us: Some(1_000_000),
                ..load()
            },
            Reason::DecoderWork,
        ),
    ] {
        let mut c = controller();
        for t in 0..=20 {
            c.update_with_receiver(sample(t * 50_000), E::Measured(measurement))
                .unwrap();
        }
        assert_eq!(c.interval_us(), 200_000);
        for d in c.decisions() {
            assert_eq!(d.reason, reason);
            assert_eq!(d.pressure_us, [0; 3]);
        }
    }
}
#[test]
fn unmeasured_or_inflight_decoder_work_never_justifies_an_upward_probe() {
    for evidence in [
        E::Unknown,
        E::Measured(Load {
            decoding: true,
            work_us: Some(0),
            ..load()
        }),
        E::Measured(Load {
            work_us: None,
            ..load()
        }),
    ] {
        let mut c = controller();
        for t in 0..=100 {
            let r = c
                .update_with_receiver(sample(t * 50_000), evidence)
                .unwrap();
            assert_eq!(r.headroom_us, 0);
            assert_ne!(r.reason, Reason::HeadroomProbe);
        }
        assert_eq!(c.interval_us(), 40_000);
    }
    let mut c = controller();
    for t in 0..=50 {
        c.update_with_receiver(sample(t * 50_000), E::Measured(load()))
            .unwrap();
    }
    assert!(c.interval_us() < 40_000);
}
#[test]
fn expired_remote_pressure_cannot_accumulate_dwell_or_enable_a_probe() {
    let mut h = Requester::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap();
    h.prepare(0).unwrap();
    h.queued(0).unwrap();
    h.receive(&reply(1), 0).unwrap();
    let mut c = controller();
    for t in 0..=60 {
        let now = t * 50_000;
        c.update_with_receiver(sample(now), h.evidence(now).unwrap())
            .unwrap();
    }
    assert_eq!(c.interval_us(), 40_000);
    assert!(c.decisions().next().is_none());
}
#[test]
fn decoder_pressure_prevents_an_idle_wake_from_overriding_known_load() {
    let mut c = controller();
    for t in 0..=20 {
        let mut s = sample(t * 50_000);
        s.observation.as_mut().unwrap().changed = false;
        c.update_with_receiver(s, E::Measured(load())).unwrap();
    }
    let r = c
        .update_with_receiver(
            sample(1_050_000),
            E::Measured(Load {
                work_us: Some(500_000),
                ..load()
            }),
        )
        .unwrap();
    assert_eq!(r.reason, Reason::ChangedAfterIdle);
    assert_eq!(r.interval_us, 200_000);
}

/// Two bounded in-flight records and independent clocks exercise the actual
/// single-credit query/reply state machines, including their original expiry.
fn delayed_feedback(rtt_us: u64, measured: Load, end_us: u64) -> Vec<fr_media::pacing::Report> {
    let mut host = Requester::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap();
    let viewer_epoch = 90_000_000;
    let mut viewer = Responder::new(binding(), ProtocolLimits::ABSOLUTE, viewer_epoch).unwrap();
    let mut controller = controller();
    let mut query: Option<(u64, Vec<u8>, u64)> = None;
    let mut reply: Option<(u64, Vec<u8>, u64)> = None;
    let mut reports = Vec::new();
    for now in (0..=end_us).step_by(10_000) {
        if query.as_ref().is_some_and(|record| record.0 <= now) {
            let (_, bytes, original_until) = query.take().unwrap();
            let viewer_now = viewer_epoch + now;
            assert!(viewer.receive(&bytes, measured, viewer_now).unwrap());
            assert!(
                viewer
                    .transmit(viewer_now, |bytes, until| {
                        assert_eq!(until, viewer_now + REPLY_LIFETIME_US);
                        assert!(reply.is_none());
                        reply = Some((now + rtt_us / 2, bytes.to_vec(), original_until));
                        Ok::<_, ()>(Admission::Accepted)
                    })
                    .unwrap()
            );
        }
        if reply.as_ref().is_some_and(|record| record.0 <= now) {
            let (_, bytes, original_until) = reply.take().unwrap();
            let accepted = host.receive(&bytes, now).unwrap();
            assert_eq!(accepted, now < original_until);
            if accepted {
                assert_eq!(host.sample(now).unwrap(), Some((measured, original_until)));
            }
        }
        host.transmit(now, |bytes, until| {
            assert_eq!(until, now + SAMPLE_LIFETIME_US);
            assert!(query.is_none());
            query = Some((now + rtt_us / 2, bytes.to_vec(), until));
            Ok::<_, ()>(Admission::Accepted)
        })
        .unwrap();
        let evidence = host.evidence(now).unwrap();
        let report = controller
            .update_with_receiver(sample(now), evidence)
            .unwrap();
        if evidence == E::Unknown {
            assert_eq!(report.receiver_pressure_us, [0; 2]);
            assert!(!matches!(
                report.reason,
                Reason::ReceiverBacklog | Reason::DecoderWork
            ));
        }
        reports.push(report);
    }
    reports
}

#[test]
fn live_high_rtt_replies_accumulate_only_measured_pressure_between_query_gaps() {
    for (measured, reason, component) in [
        (
            Load {
                retained_pictures: 3,
                ..load()
            },
            Reason::ReceiverBacklog,
            0,
        ),
        (
            Load {
                work_us: Some(1_000_000),
                ..load()
            },
            Reason::DecoderWork,
            1,
        ),
    ] {
        let reports = delayed_feedback(120_000, measured, 2_000_000);
        for now in [150_000, 230_000, 270_000, 350_000] {
            let report = reports
                .iter()
                .find(|report| report.sample.now_us == now)
                .unwrap();
            assert_eq!(report.receiver, E::Unknown, "query expiry is not extended");
            assert_eq!(report.receiver_pressure_us, [0; 2]);
        }
        let decisions: Vec<_> = reports
            .iter()
            .filter(|report| report.reason == reason)
            .collect();
        let first = decisions
            .first()
            .expect("live delayed overload must eventually reduce capture");
        // Each 30 ms live reply window supplies two measured 10 ms edges.
        // Five windows are needed; none of the 90 ms query gaps is counted.
        assert_eq!(first.sample.now_us, 620_000);
        assert_eq!(first.receiver_pressure_us[component], 100_000);
        assert_eq!(
            (first.previous_interval_us, first.interval_us),
            (40_000, 80_000)
        );
        for pair in decisions.windows(2) {
            assert!(pair[1].sample.now_us - pair[0].sample.now_us >= 250_000);
        }
        assert_eq!(reports.last().unwrap().interval_us, 200_000);
    }
}

#[test]
fn late_replies_and_isolated_positive_endpoints_never_bank_pressure() {
    let measured = Load {
        work_us: Some(1_000_000),
        ..load()
    };
    let reports = delayed_feedback(160_000, measured, 2_000_000);
    assert!(reports.iter().all(|report| report.receiver == E::Unknown));
    assert!(reports.iter().all(|report| report.interval_us == 40_000));
    let mut c = controller();
    for now in (0..=1_000_000_u64).step_by(10_000) {
        let evidence = if now.is_multiple_of(60_000) {
            E::Measured(measured)
        } else {
            E::Unknown
        };
        let report = c.update_with_receiver(sample(now), evidence).unwrap();
        assert_eq!(report.receiver_pressure_us, [0; 2]);
        assert_eq!(report.reason, Reason::Hold);
    }
}

#[test]
fn receiver_pressure_gaps_pause_only_within_the_last_positive_bound() {
    let measured = E::Measured(Load {
        retained_pictures: 3,
        ..load()
    });
    for gap in [100_000, 100_001] {
        let mut c = controller();
        for now in [0, 40_000, 80_000] {
            c.update_with_receiver(sample(now), measured).unwrap();
        }
        for now in (90_000..80_000 + gap).step_by(10_000) {
            let report = c.update_with_receiver(sample(now), E::Unknown).unwrap();
            assert_eq!(report.receiver_pressure_us, [0; 2]);
            assert_eq!(report.reason, Reason::Hold);
        }
        let resumed = c
            .update_with_receiver(sample(80_000 + gap), measured)
            .unwrap();
        assert_eq!(
            resumed.receiver_pressure_us[0],
            if gap == 100_000 { 80_000 } else { 0 }
        );
        assert_eq!(resumed.reason, Reason::Hold);
        let next = c
            .update_with_receiver(sample(100_000 + gap), measured)
            .unwrap();
        if gap == 100_000 {
            assert_eq!(next.receiver_pressure_us[0], 100_000);
            assert_eq!(next.reason, Reason::ReceiverBacklog);
        } else {
            assert_eq!(next.receiver_pressure_us[0], 20_000);
            assert_eq!(next.reason, Reason::Hold);
        }
    }
}

#[test]
fn known_nonpressured_and_unnegotiated_evidence_clear_the_pressure_bank() {
    let busy = E::Measured(Load {
        retained_pictures: 3,
        work_us: Some(1_000_000),
        ..load()
    });
    for counterevidence in [
        E::Measured(load()),
        E::Unobserved,
        E::Measured(Load {
            work_us: None,
            ..load()
        }),
    ] {
        let mut c = controller();
        for now in [0, 40_000, 80_000] {
            c.update_with_receiver(sample(now), busy).unwrap();
        }
        c.update_with_receiver(sample(90_000), counterevidence)
            .unwrap();
        assert_eq!(
            c.update_with_receiver(sample(100_000), busy)
                .unwrap()
                .receiver_pressure_us,
            [0; 2]
        );
        let report = c.update_with_receiver(sample(120_000), busy).unwrap();
        assert_eq!(report.receiver_pressure_us, [20_000; 2]);
        assert_eq!(report.reason, Reason::Hold);
    }
}

#[test]
fn receiver_scope_reset_preserves_local_pressure_and_its_precedence() {
    let busy = E::Measured(Load {
        retained_pictures: 3,
        work_us: Some(1_000_000),
        ..load()
    });
    for (component, reason) in [
        (0, Reason::SourceWork),
        (1, Reason::SendAdmission),
        (2, Reason::CaptureCredit),
    ] {
        let mut c = controller();
        let local = |now| {
            let mut s = sample(now);
            match component {
                0 => s.source_work_us = Some(1_000_000),
                1 => s.send = Availability::Blocked,
                _ => s.capture_credit = Availability::Blocked,
            }
            s
        };
        for now in [0, 40_000, 80_000] {
            c.update_with_receiver(local(now), busy).unwrap();
        }
        c.reset_receiver_evidence();
        assert_eq!(c.interval_us(), 40_000);
        let report = c.update_with_receiver(local(100_000), busy).unwrap();
        assert_eq!(report.receiver_pressure_us, [0; 2]);
        assert_eq!(report.pressure_us[component], 100_000);
        assert_eq!(report.reason, reason);
    }
}

#[test]
fn scope_reset_discards_old_probe_evidence_but_counterevidence_does_not() {
    for scope_changed in [false, true] {
        let mut c = controller();
        for now in (0..=1_900_000).step_by(50_000) {
            c.update_with_receiver(sample(now), E::Measured(load()))
                .unwrap();
        }
        if scope_changed {
            c.reset_receiver_evidence();
        } else {
            c.reset_receiver_pressure();
        }
        let report = c
            .update_with_receiver(sample(2_000_000), E::Measured(load()))
            .unwrap();
        if scope_changed {
            assert_eq!(report.headroom_us, 0);
            assert_eq!(report.reason, Reason::Hold);
            assert_eq!(report.interval_us, 40_000);
        } else {
            assert_eq!(report.headroom_us, 2_000_000);
            assert_eq!(report.reason, Reason::HeadroomProbe);
        }
    }
}

#[test]
fn receiver_scope_reset_keeps_the_existing_reduction_cooldown() {
    let measured = E::Measured(Load {
        work_us: Some(1_000_000),
        ..load()
    });
    let mut c = controller();
    for now in [0, 50_000, 100_000] {
        c.update_with_receiver(sample(now), measured).unwrap();
    }
    assert_eq!(c.interval_us(), 80_000);
    c.reset_receiver_evidence();
    for now in [110_000, 150_000, 200_000, 210_000, 250_000, 300_000] {
        let report = c.update_with_receiver(sample(now), measured).unwrap();
        assert_eq!(report.interval_us, 80_000);
        assert_eq!(report.reason, Reason::Hold);
    }
    let report = c.update_with_receiver(sample(350_000), measured).unwrap();
    assert_eq!(report.reason, Reason::DecoderWork);
    assert_eq!(report.interval_us, 160_000);
}
