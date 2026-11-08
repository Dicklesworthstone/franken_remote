//! Shared real TLS/UDP wiring for terminal-custody tests. `local` is the
//! terminal producer; the constructors preserve its actual client/server role.
//! Authority, fences and report contents remain explicit test fixtures.
use crate::support::{both, clock, native_pair};
use asupersync::{cx::Cx, net::quic_native::NativeQuicUdpConnection};
use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_transport::quic::*;
use fr_wire::{
    FrameDescriptor, MediaLimits, RecoveryChunk,
    authority::Binding,
    closure::{self, Cleanup, CloseRequest, Closed, ClosedReason, OutstandingEffects},
    input::{InputDelivery, InputDirection},
    lease_revoked::{self, CleanupStage, EffectStage, Reason, Revoked},
};
use std::{cell::Cell, future::Future, time::Duration};

pub struct Pair<P = QuicRecords> {
    pub local: QuicRecords,
    pub peer: P,
    pub control: StreamRoute,
    pub incoming: StreamRoute,
    pub bulk: StreamRoute,
    pub video: DatagramRoute,
}
pub fn from_native_pair(
    cx: &Cx,
    mut local: NativeQuicUdpConnection,
    mut peer: NativeQuicUdpConnection,
) -> Pair<NativeQuicUdpConnection> {
    let control = StreamRoute {
        stream: local.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 7,
        messages: Messages::SessionControl,
        priority: Priority::Critical,
        outbound: true,
        maximum: 1150,
    };
    let incoming = StreamRoute {
        stream: peer.connection_mut().open_uni_stream(cx).unwrap(),
        outbound: false,
        ..control
    };
    let bulk = StreamRoute {
        stream: local.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 8,
        messages: Messages::Exact(0x0032),
        priority: Priority::Bulk,
        outbound: true,
        maximum: 65536,
    };
    let video = DatagramRoute {
        binding: 9,
        kind: 0x0034,
        outbound: true,
    };
    let local = QuicRecords::new(
        local,
        cx,
        &[control, incoming, bulk],
        &[video],
        Policy::default(),
    )
    .unwrap();
    Pair {
        local,
        peer,
        control,
        incoming,
        bulk,
        video,
    }
}
pub async fn raw_host(cx: &Cx) -> Pair<NativeQuicUdpConnection> {
    let (client, server) = native_pair(cx, "localhost", ALPN).await;
    from_native_pair(cx, server.unwrap(), client.unwrap())
}
pub async fn raw_client(cx: &Cx) -> Pair<NativeQuicUdpConnection> {
    let (client, server) = native_pair(cx, "localhost", ALPN).await;
    from_native_pair(cx, client.unwrap(), server.unwrap())
}
pub async fn host(cx: &Cx) -> Pair {
    raw_host(cx).await.into_records(cx)
}
impl<P> Pair<P> {
    pub fn routes(&self) -> ControlRoutes {
        ControlRoutes {
            outbound: self.control,
            inbound: self.incoming,
        }
    }
}
impl Pair<NativeQuicUdpConnection> {
    pub fn into_records(self, cx: &Cx) -> Pair {
        let streams = [self.control, self.incoming, self.bulk].map(reverse);
        let video = DatagramRoute {
            outbound: false,
            ..self.video
        };
        let peer = QuicRecords::new(self.peer, cx, &streams, &[video], Policy::default()).unwrap();
        Pair {
            local: self.local,
            peer,
            control: self.control,
            incoming: self.incoming,
            bulk: self.bulk,
            video: self.video,
        }
    }
}
fn reverse(route: StreamRoute) -> StreamRoute {
    StreamRoute {
        outbound: !route.outbound,
        ..route
    }
}
pub fn routes(pair: &Pair) -> ControlRoutes {
    ControlRoutes {
        outbound: reverse(pair.incoming),
        inbound: reverse(pair.control),
    }
}
pub fn binding() -> Binding {
    Binding {
        channel: 7,
        session: RemoteSessionId::from_raw(1),
    }
}
pub fn report() -> Revoked {
    Revoked {
        lease: InputLeaseId::from_raw(2),
        reason: Reason::LocalRevoke,
        cleanup: CleanupStage::Fenced,
        effects: EffectStage::Unknown,
    }
}
pub fn request() -> CloseRequest {
    CloseRequest {
        reason: closure::Reason::Requested,
    }
}
pub fn final_report() -> Closed {
    Closed {
        reason: ClosedReason::ClientRequested,
        cleanup: Cleanup::Unconfirmed,
        effects: OutstandingEffects::Unknown,
    }
}
pub fn request_bytes(reason: closure::Reason) -> Vec<u8> {
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
pub fn decode_revocation(bytes: &[u8]) -> Revoked {
    assert_eq!(bytes.len(), lease_revoked::REVOKED_BYTES);
    lease_revoked::decode(
        bytes,
        binding(),
        report().lease,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap()
}
pub fn decode_closed(bytes: &[u8]) -> Closed {
    assert_eq!(bytes.len(), closure::CLOSED_BYTES);
    closure::decode_closed(
        bytes,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap()
}
pub fn recovery_bytes(length: usize) -> Vec<u8> {
    let mut bytes = vec![0; 65536];
    let data = vec![99; length];
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 65536, 16384, 64).unwrap();
    let n = fr_wire::encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: u32::try_from(length).unwrap(),
            offset: 0,
            capture_micros: 1,
            bytes: &data,
        },
        8,
        &limits,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}
pub fn bulk() -> Vec<u8> {
    recovery_bytes(8000)
}
pub fn queued_video<P>(pair: &mut Pair<P>, cx: &Cx) {
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap();
    let mut bytes = [0; 1150];
    let n = fr_wire::encode_fragment(
        fr_wire::Fragment {
            descriptor: FrameDescriptor {
                frame: 0,
                reference: None,
                total_bytes: 4,
                stride: 4,
                capture_micros: 1,
            },
            index: 0,
            bytes: b"data",
        },
        pair.video.binding,
        &limits,
        &mut bytes,
    )
    .unwrap();
    // Native DATAGRAM custody without a network drive or post-fence flush.
    pair.local
        .send(
            cx,
            Route::Datagram(pair.video),
            &bytes[..n],
            clock(cx) + 1_000_000,
            || true,
        )
        .unwrap();
}
pub async fn observe_close(pair: &mut Pair<NativeQuicUdpConnection>, cx: &Cx, code: u64) {
    let until = clock(cx) + 1_000_000;
    for _ in 0..500 {
        if pair.peer.connection().close_was_peer_initiated() {
            assert_eq!(
                pair.peer.connection().inner().transport().close_code(),
                Some(code)
            );
            return;
        }
        assert!(clock(cx) < until, "peer received no protected close");
        pair.peer
            .drive_io_once(cx, Duration::from_millis(5))
            .await
            .unwrap();
        assert!(
            pair.peer.connection_mut().recv_datagram().is_none(),
            "native queued application datagram escaped after the fence"
        );
    }
    panic!("peer close exceeded its bounded receive turns");
}
pub fn no_application_bytes(pair: &mut Pair<NativeQuicUdpConnection>) {
    assert!(
        pair.peer.connection_mut().recv_datagram().is_none(),
        "native queued application datagram escaped after the fence"
    );
    for route in [pair.control, pair.bulk] {
        if let Ok(stream) = pair
            .peer
            .connection()
            .inner()
            .streams()
            .stream(route.stream)
        {
            assert_eq!(stream.recv_offset, 0, "unstaged application bytes escaped");
        }
    }
}
pub async fn observe_silence(pair: &mut Pair<NativeQuicUdpConnection>, cx: &Cx) {
    pair.peer
        .drive_io_once(cx, Duration::from_millis(10))
        .await
        .unwrap();
    assert!(!pair.peer.connection().close_was_peer_initiated());
    no_application_bytes(pair);
}
/// Drive only the original peer until its terminal sender finishes. Decode and
/// scenario assertions remain explicit; every received record must use the
/// original control stream, never a payload lane.
pub async fn collect<T, F: Future>(
    cx: &Cx,
    pair: &mut Pair,
    end: F,
    mut decode: impl FnMut(&[u8]) -> T,
) -> (F::Output, Vec<T>) {
    let done = Cell::new(false);
    let mut received = Vec::new();
    let (outcome, ()) = Box::pin(both(
        async {
            let result = end.await;
            done.set(true);
            result
        },
        async {
            while !done.get() {
                pair.peer
                    .drive(cx, Duration::from_millis(1), || true)
                    .await
                    .unwrap();
                pair.peer
                    .receive(
                        cx,
                        || true,
                        |route, bytes| {
                            assert_eq!(route, Route::Stream(reverse(pair.control)));
                            received.push(decode(bytes));
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
            }
        },
    ))
    .await;
    (outcome, received)
}
pub async fn receive_request(cx: &Cx, server: &mut QuicRecords, routes: ControlRoutes, until: u64) {
    let mut requests = 0;
    for _ in 0..500 {
        assert!(
            clock(cx) < until,
            "host never received the original close request"
        );
        server
            .drive(cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        server
            .receive(
                cx,
                || true,
                |route, bytes| {
                    assert_eq!(route, Route::Stream(routes.inbound));
                    assert_eq!(
                        closure::decode_request(
                            bytes,
                            binding(),
                            &ProtocolLimits::ABSOLUTE,
                            InputDirection::ViewerToHost,
                            InputDelivery::Reliable
                        )
                        .unwrap(),
                        request()
                    );
                    requests += 1;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if requests != 0 {
            assert_eq!(requests, 1);
            return;
        }
    }
    panic!("host close request exceeded its bounded receive turns");
}
pub async fn request_then_report(cx: &Cx, pair: &mut Pair, outcome: Closed) -> Result<(), Error> {
    let routes = pair.routes();
    receive_request(cx, &mut pair.local, routes, clock(cx) + 1_000_000).await;
    pair.local
        .close_with_closed(
            cx,
            &pair.local.binding(),
            routes.outbound,
            binding(),
            outcome,
        )
        .await
}
