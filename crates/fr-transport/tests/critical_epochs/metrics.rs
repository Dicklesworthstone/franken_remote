//! Actual UDP/TLS reservation for canonical decoder/quality and renewal records.
//! Application producers and authority are fixtures; this does not qualify
//! live requester cadence, renewed authority, a native decoder or impaired WANs.
use super::{Disposition, Error, Policy, Route, clock, runtime, support};
use asupersync::cx::Cx;
use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, InputLeaseId,
        OsSessionId, RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    limits::ProtocolLimits,
};
use fr_transport::quic::{ALPN, QuicRecords};
use fr_wire::{
    authority, decoder,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    quality_decision::{self, Decision, Mode, Reason},
    receiver_metrics::{self, Load},
};
use std::time::Duration;

fn binding() -> decoder::Binding {
    decoder::Binding {
        parent: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(13),
        },
        display: 14,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}

fn metric(direction: InputDirection, sequence: u64) -> Vec<u8> {
    let message = match direction {
        InputDirection::HostToViewer => receiver_metrics::Message::Query { sequence },
        InputDirection::ViewerToHost => receiver_metrics::Message::Reply {
            sequence,
            load: Load {
                retained_bytes: 0,
                retained_pictures: 0,
                decoding: false,
                work_us: None,
            },
        },
    };
    let mut bytes = vec![0; receiver_metrics::REPLY_BYTES];
    let length = receiver_metrics::encode(
        message,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        direction,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes.truncate(length);
    bytes
}

fn quality(direction: InputDirection, sequence: u64) -> Vec<u8> {
    assert_eq!(direction, InputDirection::HostToViewer);
    let mut bytes = vec![0; quality_decision::RECORD_BYTES];
    let length = quality_decision::encode(
        Decision {
            sequence,
            capture_interval_us: 40_000,
            mode: Mode::Active,
            reason: Reason::SendAdmission,
            applied_bitrate_bps: Some(2_000_000),
            pending_bitrate_bps: Some(1_000_000),
        },
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        direction,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(length, 150);
    assert_eq!(length, bytes.len());
    bytes
}

fn renewal(direction: InputDirection, scope: authority::Scope, until: u64) -> Vec<u8> {
    let message = match direction {
        InputDirection::HostToViewer => authority::Message::Challenge {
            scope,
            nonce: 19,
            deadline_micros: until,
        },
        InputDirection::ViewerToHost => authority::Message::Response { scope, nonce: 19 },
    };
    let mut bytes = vec![0; authority::MAX_AUTHORITY_BYTES];
    let length = authority::encode(
        message,
        authority::Binding {
            channel: binding().parent.id,
            session: binding().parent.remote_session,
        },
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        direction,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes.truncate(length);
    bytes
}

async fn check_reservation(
    cx: &Cx,
    policy: Policy,
    direction: InputDirection,
    records: usize,
    record: fn(InputDirection, u64) -> Vec<u8>,
) {
    let (client, host) = support::native_pair(cx, "localhost", ALPN).await;
    let (mut client, cr) = QuicRecords::bootstrap(client.unwrap(), cx, policy).unwrap();
    let (mut host, hr) = QuicRecords::bootstrap(host.unwrap(), cx, policy).unwrap();
    let cr = client.bind_control(cx, cr, 7, 4096, || true).unwrap();
    let hr = host.bind_control(cx, hr, 7, 4096, || true).unwrap();
    let (mut sender, mut peer, outgoing, incoming) = match direction {
        InputDirection::HostToViewer => (host, client, hr.outbound, cr.inbound),
        InputDirection::ViewerToHost => (client, host, cr.outbound, hr.inbound),
    };
    let original = sender.binding();
    let outgoing = Route::Stream(outgoing);
    let incoming = Route::Stream(incoming);
    let until = clock(cx) + 2_000_000;
    let mut expected = Vec::new();
    for sequence in 1..=records {
        let bytes = record(direction, u64::try_from(sequence).unwrap());
        sender.send(cx, outgoing, &bytes, until, || true).unwrap();
        expected.push(bytes);
        if sequence == 1 {
            // Keep the original native-unacknowledged charge while filling
            // the queue. The peer has not been serviced; no ACK is fabricated.
            sender.drive(cx, Duration::ZERO, || true).await.unwrap();
        }
    }
    assert_eq!(sender.usage().critical_send_records, records);
    assert_eq!(
        sender.usage().critical_send_bytes,
        expected.iter().map(Vec::len).sum::<usize>()
    );
    let before = sender.usage();
    assert_eq!(
        sender.send(cx, outgoing, &record(direction, 100), until, || true),
        Err(Error::Backpressure)
    );
    assert_eq!(sender.usage(), before);
    assert!(!sender.is_closed());
    for scope in [
        authority::Scope::Observation,
        authority::Scope::Control(InputLeaseId::from_raw(17)),
    ] {
        let bytes = renewal(direction, scope, until);
        sender.send(cx, outgoing, &bytes, until, || true).unwrap();
        expected.push(bytes);
    }
    assert_eq!(sender.usage().critical_send_records, records + 2);
    assert!(sender.usage().critical_send_bytes <= policy.critical_send_bytes);
    if records + 2 == policy.critical_send_records {
        assert_eq!(
            sender.send(cx, outgoing, &expected[records], until, || true),
            Err(Error::Backpressure)
        );
    }
    let mut received = Vec::new();
    for _ in 0..500 {
        let (a, b) = Box::pin(support::both(
            sender.drive(cx, Duration::from_millis(1), || true),
            peer.drive(cx, Duration::from_millis(1), || true),
        ))
        .await;
        a.unwrap();
        b.unwrap();
        peer.receive(
            cx,
            || true,
            |route, bytes| {
                assert_eq!(route, incoming);
                received.push(bytes.to_vec());
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
        if received.len() == expected.len() && sender.usage().critical_send_records == 0 {
            break;
        }
    }
    assert_eq!(received, expected);
    assert_eq!(sender.usage().critical_send_records, 0);
    assert_eq!(sender.usage().critical_send_bytes, 0);
    assert!(sender.is_bound_to(&original));
    assert!(!sender.is_closed());
}

#[test]
fn queries_and_replies_leave_both_renewal_record_slots_on_the_real_control_pair() {
    for direction in [InputDirection::HostToViewer, InputDirection::ViewerToHost] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            check_reservation(&cx, Policy::default(), direction, 14, metric).await;
        });
    }
}

#[test]
fn byte_pressure_leaves_renewal_room_before_the_record_limit_is_reached() {
    for (direction, records) in [
        (InputDirection::HostToViewer, 27),
        (InputDirection::ViewerToHost, 23),
    ] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let policy = Policy {
                critical_send_bytes: 4096,
                critical_send_records: 128,
                ..Policy::default()
            };
            check_reservation(&cx, policy, direction, records, metric).await;
        });
    }
}

#[test]
fn quality_decisions_leave_two_renewals_on_the_actual_session_control_pair() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        // The first report enters a native, unacknowledged flight. Fourteen
        // retained reports consume all advisory slots in the default budget;
        // the observation and control challenges must still both be admitted.
        check_reservation(
            &cx,
            Policy::default(),
            InputDirection::HostToViewer,
            14,
            quality,
        )
        .await;
    });
}

#[test]
fn quality_decisions_preserve_the_512_byte_reserve_before_count_pressure() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let policy = Policy {
            critical_send_bytes: 4096,
            critical_send_records: 128,
            ..Policy::default()
        };
        // Twenty-three 150-byte reports use 3450 bytes. A twenty-fourth would
        // use 3600 and cross the 3584-byte advisory ceiling even though the
        // full 4096-byte budget and 128-record allowance still have capacity.
        // The same original stream then carries both authority challenges and
        // ACK reclamation restores every original byte/record charge.
        check_reservation(&cx, policy, InputDirection::HostToViewer, 23, quality).await;
    });
}
