//! Real UDP/TLS, with no out-of-band client CID supplied to the server. The CA
//! is ephemeral test PKI; loopback is NOT tailnet ingress/admission evidence.
#[allow(dead_code)]
mod support;
use asupersync::{
    cx::Cx,
    net::{
        quic_core::ConnectionId,
        quic_native::{
            NativeQuicConnectionConfig, NativeQuicUdpConnection, QuicUdpEndpoint,
            QuicUdpEndpointConfig, StreamId,
            handshake_driver::{QuicHandshakeDriver, client_config, server_config},
        },
    },
    time::{sleep, timeout},
    types::CancelKind,
};
use fr_transport::{
    native_accept::{self as accept, Configuration, Error, Listener},
    quic::ALPN,
};
use std::{
    cell::Cell,
    fs,
    future::{Future, poll_fn},
    net::UdpSocket,
    pin::pin,
    task::Poll,
    time::Duration,
};
fn id(bytes: &[u8]) -> ConnectionId {
    ConnectionId::new(bytes).unwrap()
}
fn config() -> Configuration {
    Configuration {
        initial_timeout: Duration::from_secs(3),
        handshake_timeout: Duration::from_secs(3),
        ..Configuration::default()
    }
}
fn server(parameters: Vec<u8>) -> Result<QuicHandshakeDriver, Error> {
    let dir = support::pki();
    let tls = server_config(
        vec![fs::read(dir.join("leaf.der")).unwrap().into()],
        fs::read(dir.join("key.der")).unwrap().try_into().unwrap(),
        vec![ALPN.to_vec()],
    )
    .unwrap();
    QuicHandshakeDriver::server(tls, parameters).map_err(|_| Error::IdentityUnavailable)
}
async fn client(
    cx: &Cx,
    address: std::net::SocketAddr,
    name: &str,
    alpn: &[u8],
) -> Result<NativeQuicUdpConnection, asupersync::net::quic_native::NativeQuicUdpConnectionError> {
    let tls = client_config(
        vec![fs::read(support::pki().join("ca.der")).unwrap().into()],
        vec![alpn.to_vec()],
    )
    .unwrap();
    let driver = QuicHandshakeDriver::client(
        tls,
        name.to_owned().try_into().unwrap(),
        accept::transport_parameters(),
    )
    .unwrap();
    let endpoint = QuicUdpEndpoint::bind(
        cx,
        "127.0.0.1:0".parse().unwrap(),
        QuicUdpEndpointConfig::default(),
    )
    .await
    .unwrap();
    NativeQuicUdpConnection::connect(
        cx,
        endpoint,
        address,
        driver,
        id(b"unknown-client-dcid"),
        id(b"unknown-client-scid"),
        NativeQuicConnectionConfig {
            max_local_bidi: 0,
            max_local_uni: 8,
            send_window: 65_536,
            recv_window: 65_536,
            connection_send_limit: 524_288,
            connection_recv_limit: 524_288,
            max_datagram_frame_size: 1200,
            ..Default::default()
        },
        alpn,
    )
    .await
}
#[test]
fn discovers_unknown_peer_completes_real_tls_and_delivers_authenticated_stream() {
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let listener = Listener::bind(&cx, "127.0.0.1:0".parse().unwrap(), config())
            .await
            .unwrap();
        let address = listener.local_addr();
        let calls = Cell::new(0);
        let accept = listener
            .accept(&cx, id(b"server-local-cid"), |p| {
                calls.set(calls.get() + 1);
                server(p)
            })
            .unwrap();
        let (client, server) = Box::pin(support::both(
            timeout(
                cx.now(),
                Duration::from_secs(4),
                client(&cx, address, "localhost", ALPN),
            ),
            accept,
        ))
        .await;
        let (mut client, mut server) = (client.unwrap().unwrap(), server.unwrap());
        assert_eq!(calls.get(), 1);
        assert_eq!(server.local_addr(), address);
        assert_eq!(server.peer_addr(), client.local_addr());
        assert_eq!(server.peer_connection_id(), client.local_connection_id());
        assert_eq!(server.negotiated_alpn(), ALPN);
        let stream = client.connection_mut().open_uni_stream(&cx).unwrap();
        assert_eq!(stream, StreamId(2));
        client
            .connection_mut()
            .write_stream(
                &cx,
                stream,
                asupersync::bytes::Bytes::from_static(b"authenticated application bytes"),
                false,
            )
            .unwrap();
        client.flush(&cx).await.unwrap();
        let mut received = Vec::new();
        for _ in 0..20 {
            server
                .drive_io_once(&cx, Duration::from_millis(5))
                .await
                .unwrap();
            if let Ok(bytes) = server.connection_mut().read_stream(&cx, stream, 64) {
                received.extend_from_slice(&bytes);
                if received.len() == 31 {
                    break;
                }
            }
        }
        assert_eq!(received, b"authenticated application bytes");
        drop(server);
        drop(client);
        assert!(UdpSocket::bind(address).is_ok());
    });
}
#[test]
fn arbitrary_datagram_flood_has_a_fixed_budget_and_never_opens_tls() {
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let cfg = Configuration {
            max_initial_datagrams: 4,
            ..config()
        };
        let listener = Listener::bind(&cx, "127.0.0.1:0".parse().unwrap(), cfg)
            .await
            .unwrap();
        let addr = listener.local_addr();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        for _ in 0..8 {
            socket.send_to(&[0xff; 1200], addr).unwrap();
        }
        let result = listener
            .accept(&cx, id(b"server-local-cid"), |_| {
                panic!("noise must not reach TLS")
            })
            .unwrap()
            .await;
        assert!(matches!(result, Err(Error::InitialBudget)));
        assert!(UdpSocket::bind(addr).is_ok());
    });
}
#[test]
fn acquisition_deadline_starts_before_first_poll_and_closes_socket() {
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let listener = Listener::bind(
            &cx,
            "127.0.0.1:0".parse().unwrap(),
            Configuration {
                initial_timeout: Duration::from_millis(2),
                ..config()
            },
        )
        .await
        .unwrap();
        let address = listener.local_addr();
        let future = listener
            .accept(&cx, id(b"server-local-cid"), |_| {
                panic!("expired before TLS")
            })
            .unwrap();
        sleep(cx.now(), Duration::from_millis(10)).await;
        assert!(matches!(future.await, Err(Error::InitialTimeout)));
        assert!(UdpSocket::bind(address).is_ok());
    });
}
#[test]
fn unpolled_accept_drop_releases_original_endpoint_without_calling_factory() {
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let listener = Listener::bind(&cx, "127.0.0.1:0".parse().unwrap(), config())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let future = listener
            .accept(&cx, id(b"server-local-cid"), |_| {
                panic!("unpolled TLS factory")
            })
            .unwrap();
        drop(future);
        assert!(UdpSocket::bind(addr).is_ok());
    });
}
#[test]
fn cancellation_releases_pending_receive_and_never_calls_identity_factory() {
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let listener = Listener::bind(&cx, "127.0.0.1:0".parse().unwrap(), config())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let mut future = pin!(
            listener
                .accept(&cx, id(b"server-local-cid"), |_| panic!(
                    "cancelled before TLS"
                ))
                .unwrap()
        );
        poll_fn(|task| {
            assert!(future.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        cx.cancel_fast(CancelKind::User);
        assert!(matches!(future.await, Err(Error::Cancelled)));
        assert!(UdpSocket::bind(addr).is_ok());
    });
}
#[test]
fn wrong_server_name_and_alpn_never_return_an_authenticated_server() {
    // Complete both bounded sides; the silent side may expire after peer refusal.
    for (name, alpn) in [
        ("other.invalid", ALPN),
        ("localhost", b"unrelated-protocol".as_slice()),
    ] {
        support::runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let listener = Listener::bind(&cx, "127.0.0.1:0".parse().unwrap(), config())
                .await
                .unwrap();
            let addr = listener.local_addr();
            let future = listener
                .accept(&cx, id(b"server-local-cid"), server)
                .unwrap();
            let (client, server) = Box::pin(support::both(
                timeout(
                    cx.now(),
                    Duration::from_secs(4),
                    client(&cx, addr, name, alpn),
                ),
                future,
            ))
            .await;
            assert!(client.is_err() || client.unwrap().is_err());
            assert!(server.is_err());
            assert!(UdpSocket::bind(addr).is_ok());
        });
    }
}

fn candidate_packet() -> Vec<u8> {
    use asupersync::net::quic_core::{LongHeader, LongPacketType, PacketHeader};
    let mut packet = Vec::new();
    PacketHeader::Long(LongHeader {
        packet_type: LongPacketType::Initial,
        version: 1,
        dst_cid: id(b"unknown-client-dcid"),
        src_cid: id(b"unknown-client-scid"),
        token: vec![],
        payload_length: 1100,
        packet_number: 0,
        packet_number_len: 1,
    })
    .encode(&mut packet)
    .unwrap();
    packet.resize(1200, 0);
    packet
}
#[test]
fn stalled_candidate_expires_its_original_handshake_budget_and_releases_socket() {
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let cfg = Configuration {
            handshake_timeout: Duration::from_millis(40),
            ..config()
        };
        let listener = Listener::bind(&cx, "127.0.0.1:0".parse().unwrap(), cfg)
            .await
            .unwrap();
        let addr = listener.local_addr();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.send_to(&candidate_packet(), addr).unwrap();
        let calls = Cell::new(0);
        let result = listener
            .accept(&cx, id(b"server-local-cid"), |p| {
                calls.set(calls.get() + 1);
                server(p)
            })
            .unwrap()
            .await;
        assert!(matches!(result, Err(Error::HandshakeTimeout)));
        assert_eq!(calls.get(), 1);
        assert!(UdpSocket::bind(addr).is_ok());
    });
}
#[test]
fn another_socket_cannot_inherit_the_discovered_candidates_routing_identity() {
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let listener = Listener::bind(&cx, "127.0.0.1:0".parse().unwrap(), config())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let seed = UdpSocket::bind("127.0.0.1:0").unwrap();
        seed.send_to(&candidate_packet(), addr).unwrap();
        let accept = listener
            .accept(&cx, id(b"server-local-cid"), server)
            .unwrap();
        let (result, peer) = Box::pin(support::both(
            accept,
            timeout(
                cx.now(),
                Duration::from_secs(4),
                client(&cx, addr, "localhost", ALPN),
            ),
        ))
        .await;
        assert!(peer.unwrap().is_ok());
        assert!(matches!(result, Err(Error::PeerChanged)));
        assert!(UdpSocket::bind(addr).is_ok());
    });
}
