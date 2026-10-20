//! Actual UDP/TLS record owners; admission-clock advances are explicit fixtures.
//! No native decoder, live-tailnet or presentation evidence is claimed.
use super::*;
use asupersync::types::{Budget, CancelKind};
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_transport::quic::{ALPN, MAX_DELIVERY_ALLOWANCE_US, Policy};
use fr_wire::{
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    receiver_metrics::{self as wire, Message},
};
use std::time::Duration;

use crate::session_startup::test_network as support;

fn bytes() -> Vec<u8> {
    let binding = Binding {
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
    };
    let mut bytes = vec![0; wire::QUERY_BYTES];
    wire::encode(
        Message::Query { sequence: 1 },
        binding,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes
}
fn pair(test: impl FnOnce(&Cx, &mut QuicRecords, Route)) {
    let runtime = support::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (client, host) = support::native_pair(&cx, "localhost", ALPN).await;
        let policy = Policy {
            critical_send_records: 1,
            ..Policy::default()
        };
        let (mut host, hr) = QuicRecords::bootstrap(host.unwrap(), &cx, policy).unwrap();
        let (mut client, cr) = QuicRecords::bootstrap(client.unwrap(), &cx, policy).unwrap();
        let hr = host.bind_control(&cx, hr, 7, 4096, || true).unwrap();
        client.bind_control(&cx, cr, 7, 4096, || true).unwrap();
        test(&cx, &mut host, Route::Stream(hr.outbound));
        host.close();
        client.close();
    });
}

#[test]
fn expiry_at_either_prepared_guard_refuses_without_closing_or_charging_the_pair() {
    pair(|cx, q, route| {
        for expired_check in [1, 2] {
            let at = support::clock(cx);
            let until = at + 1_000_000;
            let before = q.usage();
            let mut checked = 0;
            assert_eq!(
                send_with_clock(q, cx, route, &bytes(), at, until, || {
                    checked += 1;
                    Ok(if checked == expired_check { until } else { at })
                }),
                Ok(Admission::Expired)
            );
            assert_eq!(checked, expired_check);
            assert!(!q.is_closed());
            assert_eq!(q.usage(), before);
        }
    });
}

#[test]
fn already_expired_unadmitted_record_is_not_a_connection_failure() {
    pair(|cx, q, route| {
        let at = support::clock(cx);
        let before = q.usage();
        assert_eq!(
            send_with_clock(q, cx, route, &bytes(), 0, at, || {
                panic!("transport rejects send_by before allocation or prepared callback")
            }),
            Ok(Admission::Expired)
        );
        assert!(!q.is_closed());
        assert_eq!(q.usage(), before);
    });
}

#[test]
fn backpressure_is_not_admission_expiry_and_does_not_run_the_prepared_guard() {
    pair(|cx, q, route| {
        let at = support::clock(cx);
        q.send(cx, route, &bytes(), at + 1_000_000, || true)
            .unwrap();
        let before = q.usage();
        assert_eq!(
            send_with_clock(q, cx, route, &bytes(), at, at + 1_000_000, || {
                panic!("no room for another record")
            }),
            Ok(Admission::Backpressure)
        );
        assert!(!q.is_closed());
        assert_eq!(q.usage(), before);
    });
}

#[test]
fn expired_retained_bytes_are_still_a_terminal_transport_failure() {
    pair(|cx, q, route| {
        let at = support::clock(cx);
        q.send(cx, route, &bytes(), at + 1_000_000, || true)
            .unwrap();
        assert_eq!(q.usage().retained_send_records, 1);
        // Before an RTT sample, admitted bytes retain the full path allowance,
        // even after their send-by usefulness ends. Keep the original record
        // unacknowledged and cross that bounded delivery deadline, not send-by.
        let retention_bound = support::clock(cx) + MAX_DELIVERY_ALLOWANCE_US;
        std::thread::sleep(Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US + 100_000));
        assert!(support::clock(cx) >= retention_bound);
        assert_eq!(q.usage().retained_send_records, 1);
        let result = send_with_clock(q, cx, route, &bytes(), at, at + 3_000_000, || {
            panic!("retained expiry is checked before prepared-record usefulness")
        });
        assert_eq!(result, Err(Error::Transport(quic::Error::Expired)));
        assert!(q.is_closed());
    });
}

#[test]
fn cancellation_dominates_a_prepared_record_that_would_have_expired() {
    pair(|cx, q, route| {
        let at = support::clock(cx);
        cx.cancel_fast(CancelKind::User);
        let result = send_with_clock(q, cx, route, &bytes(), at, at + 1_000_000, || {
            panic!("cancelled connection must not reach the record guard")
        });
        assert_eq!(result, Err(Error::Transport(quic::Error::Cancelled)));
        assert!(q.is_closed());
    });
}

#[test]
fn clock_regression_is_not_silently_discarded_as_optional_telemetry() {
    pair(|cx, q, route| {
        let at = support::clock(cx) + 1;
        assert_eq!(
            send_with_clock(q, cx, route, &bytes(), at, at + 1_000_000, || Ok(at - 1)),
            Err(Error::Clock)
        );
    });
}
