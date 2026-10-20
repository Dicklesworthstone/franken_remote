//! Production solicited-feedback owners with explicit admission outcomes.
//! These are codec/state-machine tests, not a native QUIC or pacing benchmark.
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::{
    pacing::ReceiverEvidence,
    receiver_feedback::{
        Admission, Error, REPLY_LIFETIME_US, Requester, Responder, SAMPLE_LIFETIME_US, SendError,
    },
};
use fr_wire::{
    decoder::Binding,
    input::{InputDelivery::Reliable, InputDirection::*},
    negotiation::ControlBinding,
    receiver_metrics::{self as wire, Load, Message},
};

fn binding() -> Binding {
    Binding {
        parent: ControlBinding {
            id: 7,
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
fn owners() -> (Requester, Responder) {
    (
        Requester::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap(),
        Responder::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap(),
    )
}
fn load() -> Load {
    Load {
        retained_bytes: 4096,
        retained_pictures: 2,
        decoding: true,
        work_us: Some(12_345),
    }
}
fn encode(message: Message) -> Vec<u8> {
    let mut bytes = vec![0; wire::REPLY_BYTES];
    let direction = match message {
        Message::Query { .. } => HostToViewer,
        Message::Reply { .. } => ViewerToHost,
    };
    let n = wire::encode(
        message,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        direction,
        Reliable,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}
fn sequence(bytes: &[u8], query: bool) -> u64 {
    match wire::decode(
        bytes,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        if query { HostToViewer } else { ViewerToHost },
        Reliable,
    )
    .unwrap()
    {
        Message::Query { sequence } | Message::Reply { sequence, .. } => sequence,
    }
}

#[test]
fn query_expiring_inside_admission_is_retired_without_replacement_or_evidence() {
    let (mut host, _) = owners();
    let mut calls = 0;
    assert!(
        !host
            .transmit(0, |bytes, until| {
                calls += 1;
                assert_eq!(sequence(bytes, true), 1);
                assert_eq!(until, SAMPLE_LIFETIME_US);
                Ok::<_, &'static str>(Admission::Expired)
            })
            .unwrap()
    );
    assert_eq!(
        calls, 1,
        "expiry must not prepare/send another query in this call"
    );
    assert!(host.pending(SAMPLE_LIFETIME_US).unwrap().is_none());
    assert_eq!(
        host.evidence(SAMPLE_LIFETIME_US).unwrap(),
        ReceiverEvidence::Unknown
    );
    assert!(
        !host
            .receive(
                &encode(Message::Reply {
                    sequence: 1,
                    load: load()
                }),
                SAMPLE_LIFETIME_US
            )
            .unwrap()
    );
    assert!(
        host.transmit(SAMPLE_LIFETIME_US + 1, |bytes, until| {
            assert_eq!(sequence(bytes, true), 2);
            assert_eq!(until, 2 * SAMPLE_LIFETIME_US + 1);
            Ok::<_, &'static str>(Admission::Accepted)
        })
        .unwrap()
    );
    assert!(
        !host
            .receive(
                &encode(Message::Reply {
                    sequence: 1,
                    load: load()
                }),
                SAMPLE_LIFETIME_US + 2
            )
            .unwrap()
    );
    assert!(
        host.receive(
            &encode(Message::Reply {
                sequence: 2,
                load: load()
            }),
            SAMPLE_LIFETIME_US + 2
        )
        .unwrap()
    );
}

#[test]
fn reply_expiring_inside_admission_is_not_resampled_for_the_consumed_query() {
    let (_, mut viewer) = owners();
    let query = encode(Message::Query { sequence: 1 });
    assert!(viewer.receive(&query, load(), 0).unwrap());
    let original = viewer.pending(0).unwrap().unwrap().0.to_vec();
    assert!(
        !viewer
            .transmit(0, |bytes, until| {
                assert_eq!(bytes, original);
                assert_eq!(until, REPLY_LIFETIME_US);
                Ok::<_, &'static str>(Admission::Expired)
            })
            .unwrap()
    );
    assert!(viewer.pending(REPLY_LIFETIME_US).unwrap().is_none());
    assert!(
        !viewer
            .receive(&query, load(), REPLY_LIFETIME_US + 1)
            .unwrap()
    );
    assert!(
        !viewer
            .transmit(REPLY_LIFETIME_US + 1, |_, _| -> Result<Admission, ()> {
                panic!("consumed query cannot produce a replacement sample")
            })
            .unwrap()
    );
    assert!(
        viewer
            .receive(
                &encode(Message::Query { sequence: 2 }),
                load(),
                REPLY_LIFETIME_US + 2
            )
            .unwrap()
    );
    assert!(
        viewer
            .transmit(REPLY_LIFETIME_US + 2, |bytes, _| {
                assert_eq!(sequence(bytes, false), 2);
                Ok::<_, &'static str>(Admission::Accepted)
            })
            .unwrap()
    );
}

#[test]
fn query_backpressure_keeps_exact_bytes_and_original_budget_through_retirement() {
    let (mut host, _) = owners();
    let mut original = None;
    for now in [0, 1, 50_000, SAMPLE_LIFETIME_US - 1] {
        assert!(
            !host
                .transmit(now, |bytes, until| {
                    let record = (bytes.to_vec(), until);
                    assert_eq!(original.get_or_insert_with(|| record.clone()), &record);
                    Ok::<_, &'static str>(Admission::Backpressure)
                })
                .unwrap()
        );
    }
    assert_eq!(original.unwrap().1, SAMPLE_LIFETIME_US);
    assert!(
        !host
            .transmit(SAMPLE_LIFETIME_US - 1, |_, _| Ok::<_, &'static str>(
                Admission::Expired
            ))
            .unwrap()
    );
    assert!(host.pending(SAMPLE_LIFETIME_US).unwrap().is_none());
}

#[test]
fn reply_backpressure_never_replaces_the_sample_or_restarts_its_deadline() {
    let (_, mut viewer) = owners();
    viewer
        .receive(&encode(Message::Query { sequence: 1 }), load(), 0)
        .unwrap();
    let original = viewer.pending(0).unwrap().unwrap().0.to_vec();
    for (seq, now) in [(2, 1), (3, 25_000), (4, REPLY_LIFETIME_US - 1)] {
        assert!(
            !viewer
                .receive(
                    &encode(Message::Query { sequence: seq }),
                    Load {
                        work_us: None,
                        ..load()
                    },
                    now
                )
                .unwrap()
        );
        assert!(
            !viewer
                .transmit(now, |bytes, until| {
                    assert_eq!(bytes, original);
                    assert_eq!(until, REPLY_LIFETIME_US);
                    Ok::<_, &'static str>(Admission::Backpressure)
                })
                .unwrap()
        );
    }
    assert!(
        !viewer
            .transmit(REPLY_LIFETIME_US - 1, |_, _| Ok::<_, &'static str>(
                Admission::Expired
            ))
            .unwrap()
    );
    assert!(
        !viewer
            .receive(
                &encode(Message::Query { sequence: 4 }),
                load(),
                REPLY_LIFETIME_US + 1
            )
            .unwrap()
    );
}

#[test]
fn transport_failures_are_not_expiration_or_admission_and_preserve_the_exact_record() {
    let (mut host, mut viewer) = owners();
    host.prepare(0).unwrap();
    let query = host.pending(0).unwrap().unwrap().0.to_vec();
    viewer.receive(&query, load(), 0).unwrap();
    let reply = viewer.pending(0).unwrap().unwrap().0.to_vec();
    for cause in [
        "cancelled",
        "unauthorized",
        "retained_send_expired",
        "malformed",
    ] {
        assert_eq!(
            host.transmit(0, |_, _| Err(cause)),
            Err(SendError::Transport(cause))
        );
        assert_eq!(
            viewer.transmit(0, |_, _| Err(cause)),
            Err(SendError::Transport(cause))
        );
        assert_eq!(host.pending(0).unwrap().unwrap().0, query);
        assert_eq!(viewer.pending(0).unwrap().unwrap().0, reply);
    }
    assert_eq!(
        host.receive(&reply, 0),
        Err(Error::Sequence),
        "a failed admission is not a sent query"
    );
}

#[test]
fn admitted_records_are_never_resubmitted_and_evidence_keeps_the_original_query_deadline() {
    let (mut host, mut viewer) = owners();
    let mut query = Vec::new();
    assert!(
        host.transmit(0, |bytes, _| {
            query.extend_from_slice(bytes);
            Ok::<_, &'static str>(Admission::Accepted)
        })
        .unwrap()
    );
    assert!(
        !host
            .transmit(1, |_, _| -> Result<Admission, ()> {
                panic!("duplicate query")
            })
            .unwrap()
    );
    viewer.receive(&query, load(), 50_000).unwrap();
    let mut reply = Vec::new();
    assert!(
        viewer
            .transmit(50_001, |bytes, _| {
                reply.extend_from_slice(bytes);
                Ok::<_, &'static str>(Admission::Accepted)
            })
            .unwrap()
    );
    assert!(
        !viewer
            .transmit(50_002, |_, _| -> Result<Admission, ()> {
                panic!("duplicate reply")
            })
            .unwrap()
    );
    assert!(host.receive(&reply, SAMPLE_LIFETIME_US - 1).unwrap());
    assert_eq!(
        host.evidence(SAMPLE_LIFETIME_US - 1).unwrap(),
        ReceiverEvidence::Measured(load())
    );
    assert_eq!(
        host.evidence(SAMPLE_LIFETIME_US).unwrap(),
        ReceiverEvidence::Unknown
    );
}

#[test]
fn regression_and_overflow_refuse_before_transport_or_new_state() {
    let (mut host, mut viewer) = owners();
    host.prepare(10).unwrap();
    viewer
        .receive(&encode(Message::Query { sequence: 1 }), load(), 10)
        .unwrap();
    assert_eq!(
        host.transmit(9, |_, _| -> Result<Admission, ()> {
            panic!("regressed query")
        }),
        Err(SendError::Protocol(Error::Clock))
    );
    assert_eq!(
        viewer.transmit(9, |_, _| -> Result<Admission, ()> {
            panic!("regressed reply")
        }),
        Err(SendError::Protocol(Error::Clock))
    );
    let mut overflow = Requester::new(binding(), ProtocolLimits::ABSOLUTE, u64::MAX).unwrap();
    assert_eq!(
        overflow.transmit(u64::MAX, |_, _| -> Result<Admission, ()> {
            panic!("overflow")
        }),
        Err(SendError::Protocol(Error::Clock))
    );
}

#[test]
fn expiration_during_admission_establishes_a_clock_floor_on_both_original_owners() {
    let (mut host, mut viewer) = owners();
    viewer
        .receive(&encode(Message::Query { sequence: 1 }), load(), 0)
        .unwrap();
    assert!(
        !host
            .transmit(0, |_, _| Ok::<_, ()>(Admission::Expired))
            .unwrap()
    );
    assert!(
        !viewer
            .transmit(0, |_, _| Ok::<_, ()>(Admission::Expired))
            .unwrap()
    );
    assert_eq!(
        host.transmit(SAMPLE_LIFETIME_US - 1, |_, _| -> Result<Admission, ()> {
            panic!("time cannot regress behind observed query expiry")
        }),
        Err(SendError::Protocol(Error::Clock))
    );
    assert_eq!(
        viewer.transmit(REPLY_LIFETIME_US - 1, |_, _| -> Result<Admission, ()> {
            panic!("time cannot regress behind observed reply expiry")
        }),
        Err(SendError::Protocol(Error::Clock))
    );
}
