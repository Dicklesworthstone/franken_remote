//! Real UDP/TLS and async local-factory boundaries. Synthetic CA and loopback
//! are explicit fixtures, not installed-tailnet or interface qualification.
#[allow(dead_code)]
mod support;
use asupersync::{
    cx::Cx,
    net::{
        quic_core::ConnectionId,
        quic_native::{
            NativeQuicConnectionConfig, NativeQuicUdpConnection, QuicUdpEndpoint,
            QuicUdpEndpointConfig,
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
    future::{Future, pending, poll_fn},
    net::{SocketAddr, UdpSocket},
    pin::pin,
    task::Poll,
    time::Duration,
};
fn id(value: &[u8]) -> ConnectionId {
    ConnectionId::new(value).unwrap()
}
fn config() -> Configuration {
    Configuration {
        initial_timeout: Duration::from_secs(3),
        handshake_timeout: Duration::from_secs(3),
        ..Default::default()
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
async fn client(cx: &Cx, address: SocketAddr) -> NativeQuicUdpConnection {
    let tls = client_config(
        vec![fs::read(support::pki().join("ca.der")).unwrap().into()],
        vec![ALPN.to_vec()],
    )
    .unwrap();
    let driver = QuicHandshakeDriver::client(
        tls,
        "localhost".to_owned().try_into().unwrap(),
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
        ALPN,
    )
    .await
    .unwrap()
}
async fn candidate(cx: &Cx, handshake_timeout: Duration) -> (Listener, UdpSocket, SocketAddr) {
    let listener = Listener::bind(
        cx,
        "127.0.0.1:0".parse().unwrap(),
        Configuration {
            handshake_timeout,
            ..config()
        },
    )
    .await
    .unwrap();
    let address = listener.local_addr();
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    // Valid unprotected routing prefix only. Not an authenticated Initial. Every
    // negative test below must stop before attempting TLS on these bytes.
    let mut bytes = vec![0xc0, 0, 0, 0, 1, 8];
    bytes.extend_from_slice(b"12345678");
    bytes.push(8);
    bytes.extend_from_slice(b"87654321");
    bytes.extend_from_slice(&[0, 0x44, 0x96]);
    bytes.resize(1200, 0);
    socket.send_to(&bytes, address).unwrap();
    (listener, socket, address)
}
struct DropNotice<'a>(&'a Cell<bool>);
impl Drop for DropNotice<'_> {
    fn drop(&mut self) {
        self.0.set(true);
    }
}
#[test]
fn asynchronous_identity_uses_discovered_peer_and_completes_canonical_tls() {
    // Prepare test PKI outside the bounded identity/handshake operation.
    let _ = support::pki();
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let listener = Listener::bind(&cx, "127.0.0.1:0".parse().unwrap(), config())
            .await
            .unwrap();
        let address = listener.local_addr();
        let peer = Cell::new(None);
        let calls = Cell::new(0);
        let accepted = listener
            .accept_with_identity(&cx, id(b"server-local-cid"), |candidate, parameters| {
                calls.set(calls.get() + 1);
                peer.set(Some(candidate));
                let cx = &cx;
                async move {
                    sleep(cx.now(), Duration::from_millis(20)).await;
                    server(parameters)
                }
            })
            .unwrap();
        let (client, server) = Box::pin(timeout(
            cx.now(),
            Duration::from_secs(5),
            support::both(client(&cx, address), accepted),
        ))
        .await
        .unwrap();
        let mut server = server.unwrap();
        let mut client = client;
        assert_eq!(peer.get(), Some(client.local_addr()));
        assert_eq!(calls.get(), 1);
        assert_eq!(server.peer_addr(), client.local_addr());
        assert_eq!(server.peer_connection_id(), client.local_connection_id());
        assert_eq!(server.negotiated_alpn(), ALPN);
        let stream = client.connection_mut().open_uni_stream(&cx).unwrap();
        client
            .connection_mut()
            .write_stream(
                &cx,
                stream,
                asupersync::bytes::Bytes::from_static(b"after identity and TLS"),
                false,
            )
            .unwrap();
        client.flush(&cx).await.unwrap();
        let mut received = Vec::new();
        for _ in 0..30 {
            server
                .drive_io_once(&cx, Duration::from_millis(5))
                .await
                .unwrap();
            if let Ok(bytes) = server.connection_mut().read_stream(&cx, stream, 64) {
                received.extend_from_slice(&bytes);
            }
            if received.len() == b"after identity and TLS".len() {
                break;
            }
        }
        assert_eq!(received, b"after identity and TLS");
        drop(server);
        drop(client);
        assert!(UdpSocket::bind(address).is_ok());
    });
}
#[test]
fn stalled_identity_expires_drops_its_owner_and_never_sends_tls() {
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (listener, socket, address) = candidate(&cx, Duration::from_millis(20)).await;
        let dropped = Cell::new(false);
        let result = listener
            .accept_with_identity(&cx, id(b"server-local-cid"), |peer, _| {
                assert_eq!(peer, socket.local_addr().unwrap());
                let notice = DropNotice(&dropped);
                async move {
                    let _notice = notice;
                    pending().await
                }
            })
            .unwrap()
            .await;
        assert!(matches!(result, Err(Error::HandshakeTimeout)));
        assert!(dropped.get());
        assert!(UdpSocket::bind(address).is_ok());
        socket.set_nonblocking(true).unwrap();
        assert_eq!(
            socket.recv(&mut [0; 1500]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    });
}
#[test]
fn refused_identity_preserves_typed_failure_and_releases_socket() {
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (listener, _, address) = candidate(&cx, Duration::from_secs(1)).await;
        let result = listener
            .accept_with_identity(&cx, id(b"server-local-cid"), |_, _| async {
                Err(Error::IdentityUnavailable)
            })
            .unwrap()
            .await;
        assert!(matches!(result, Err(Error::IdentityUnavailable)));
        assert!(UdpSocket::bind(address).is_ok());
    });
}
#[test]
fn ready_identity_cannot_bypass_cancellation_or_begin_tls() {
    let _ = support::pki();
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (listener, _, address) = candidate(&cx, Duration::from_secs(1)).await;
        let result = listener
            .accept_with_identity(&cx, id(b"server-local-cid"), |_, parameters| async {
                let driver = server(parameters)?;
                cx.cancel_fast(CancelKind::User);
                Ok(driver)
            })
            .unwrap()
            .await;
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(UdpSocket::bind(address).is_ok());
    });
}
#[test]
fn abandoning_pending_identity_drops_lookup_and_original_socket() {
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (listener, _, address) = candidate(&cx, Duration::from_secs(1)).await;
        let dropped = Cell::new(false);
        let entered = Cell::new(false);
        {
            let future = listener
                .accept_with_identity(&cx, id(b"server-local-cid"), |_, _| {
                    entered.set(true);
                    let notice = DropNotice(&dropped);
                    async move {
                        let _notice = notice;
                        pending().await
                    }
                })
                .unwrap();
            let mut future = pin!(future);
            poll_fn(|task| {
                assert!(future.as_mut().poll(task).is_pending());
                if entered.get() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }
        assert!(dropped.get());
        assert!(UdpSocket::bind(address).is_ok());
    });
}
#[test]
fn unpolled_async_accept_keeps_call_time_acquisition_deadline() {
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
            .accept_with_identity(&cx, id(b"server-local-cid"), |_, _| async {
                panic!("expired acquisition must not invoke the local factory")
            })
            .unwrap();
        sleep(cx.now(), Duration::from_millis(10)).await;
        assert!(matches!(future.await, Err(Error::InitialTimeout)));
        assert!(UdpSocket::bind(address).is_ok());
    });
}
#[test]
fn identity_wait_does_not_restart_the_handshake_budget() {
    let _ = support::pki();
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (listener, _, address) = candidate(&cx, Duration::from_millis(100)).await;
        let accepted = listener
            .accept_with_identity(&cx, id(b"server-local-cid"), |_, parameters| async {
                sleep(cx.now(), Duration::from_millis(75)).await;
                server(parameters)
            })
            .unwrap();
        // A renewed 100ms TLS budget would still be pending at 150ms. The real
        // total deadline is 100ms, including the local identity wait.
        let result = timeout(cx.now(), Duration::from_millis(150), accepted)
            .await
            .unwrap();
        assert!(matches!(result, Err(Error::HandshakeTimeout)));
        assert!(UdpSocket::bind(address).is_ok());
    });
}
