//! Real UDP/TLS/QUIC terminal reporting. The grant/fence is a local fixture;
//! these tests make no native-input, Tailscale, or cleanup-success claim.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod support;

use asupersync::{cx::Cx, types::CancelKind};
use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_transport::quic::*;
use fr_wire::{
    authority::Binding,
    input::{InputDelivery, InputDirection},
    lease_revoked::{self, CleanupStage, EffectStage, REVOKED_BYTES, Reason, Revoked},
};
use std::{
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use support::{both, clock, runtime};

struct Pair {
    client: QuicRecords,
    server: QuicRecords,
    control: StreamRoute,
    incoming: StreamRoute,
}
async fn pair(cx: &Cx) -> Pair {
    let (c, s) = support::native_pair(cx, "localhost", ALPN).await;
    let (mut c, mut s) = (c.unwrap(), s.unwrap());
    let control = StreamRoute {
        stream: s.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 7,
        messages: Messages::SessionControl,
        priority: Priority::Critical,
        outbound: true,
        maximum: 1150,
    };
    let bulk = StreamRoute {
        stream: s.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 8,
        messages: Messages::Exact(0x0032),
        priority: Priority::Bulk,
        outbound: true,
        maximum: 65536,
    };
    let incoming = StreamRoute {
        stream: c.connection_mut().open_uni_stream(cx).unwrap(),
        outbound: false,
        ..control
    };
    let routes = [control, bulk, incoming];
    let video = DatagramRoute {
        binding: 9,
        kind: 0x0034,
        outbound: true,
    };
    let client = QuicRecords::new(
        c,
        cx,
        &routes.map(|r| StreamRoute {
            outbound: !r.outbound,
            ..r
        }),
        &[DatagramRoute {
            outbound: false,
            ..video
        }],
        Policy::default(),
    )
    .unwrap();
    let server = QuicRecords::new(s, cx, &routes, &[video], Policy::default()).unwrap();
    Pair {
        client,
        server,
        control,
        incoming,
    }
}
fn binding() -> Binding {
    Binding {
        channel: 7,
        session: RemoteSessionId::from_raw(1),
    }
}
fn report() -> Revoked {
    Revoked {
        lease: InputLeaseId::from_raw(2),
        reason: Reason::LocalRevoke,
        cleanup: CleanupStage::Fenced,
        effects: EffectStage::Unknown,
    }
}

// Real closing exchange on UDP/TLS. Report stages remain explicit fixtures.

use fr_wire::closure::{self, Cleanup, CloseRequest, Closed, ClosedReason, OutstandingEffects};

fn routes(pair: &Pair) -> ControlRoutes {
    ControlRoutes {
        outbound: StreamRoute {
            outbound: true,
            ..pair.incoming
        },
        inbound: StreamRoute {
            outbound: false,
            ..pair.control
        },
    }
}
fn request() -> CloseRequest {
    CloseRequest {
        reason: closure::Reason::Requested,
    }
}
fn final_report() -> Closed {
    Closed {
        reason: ClosedReason::ClientRequested,
        cleanup: Cleanup::Unconfirmed,
        effects: OutstandingEffects::Unknown,
    }
}
fn request_bytes(reason: closure::Reason) -> Vec<u8> {
    let mut bytes = vec![0; closure::REQUEST_BYTES];
    closure::encode_request(
        CloseRequest { reason },
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes
}
async fn request_then_report(cx: &Cx, pair: &mut Pair, report: Closed) -> Result<(), Error> {
    let mut requests = Vec::new();
    while requests.is_empty() {
        pair.server
            .drive(cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        pair.server
            .receive(
                cx,
                || true,
                |route, bytes| {
                    assert_eq!(route, Route::Stream(pair.incoming));
                    requests.push(
                        closure::decode_request(
                            bytes,
                            binding(),
                            &ProtocolLimits::ABSOLUTE,
                            InputDirection::ViewerToHost,
                            InputDelivery::Reliable,
                        )
                        .unwrap(),
                    );
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
    assert_eq!(requests, [request()]);
    let original = pair.server.binding();
    pair.server
        .close_with_closed(cx, &original, pair.control, binding(), report)
        .await
}

#[path = "terminal_revocation/exchange/controller.rs"]
mod checks;
