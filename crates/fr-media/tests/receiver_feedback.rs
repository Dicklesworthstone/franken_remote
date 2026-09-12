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
