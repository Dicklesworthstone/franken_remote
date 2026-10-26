//! Actual IPv4/IPv6 UDP and TLS. Loopback remains explicitly unqualified ingress.
use super::*;
use asupersync::net::quic_core::{LongHeader, LongPacketType, PacketHeader};
use std::net::SocketAddr;

fn addresses() -> [SocketAddr; 2] {
    ["127.0.0.1:0".parse().unwrap(), "[::1]:0".parse().unwrap()]
}

#[test]
fn either_address_selects_one_original_endpoint_for_real_tls_and_application_bytes() {
    let _serial = serialized();
    for selected in 0..2 {
        support::runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let listener = Listener::bind_addresses(&cx, &addresses(), config())
                .await
                .unwrap();
            let bound = listener.local_addresses();
            assert_eq!(bound.len(), 2);
            let selected_local = Cell::new(None);
            let calls = Cell::new(0);
            let accepting = listener
                .accept_with_local_identity(&cx, id(b"dual-host-cid"), |local, peer, parameters| {
                    calls.set(calls.get() + 1);
                    selected_local.set(Some(local));
                    assert_eq!(local.is_ipv4(), peer.is_ipv4());
                    assert!(
                        UdpSocket::bind(bound[1 - selected]).is_ok(),
                        "unused socket must close before the identity factory and TLS"
                    );
                    assert!(
                        UdpSocket::bind(local).is_err(),
                        "selected original socket must remain owned during identity setup"
                    );
                    std::future::ready(server(parameters))
                })
                .unwrap();
            let (viewer, host) = Box::pin(support::both(
                timeout(
                    cx.now(),
                    Duration::from_secs(4),
                    client(&cx, bound[selected], "localhost", ALPN),
                ),
                accepting,
            ))
            .await;
            let (mut viewer, mut host) = (viewer.unwrap().unwrap(), host.unwrap());
            assert_eq!(calls.get(), 1);
            assert_eq!(selected_local.get(), Some(bound[selected]));
            assert_eq!(host.local_addr(), bound[selected]);
            assert_eq!(host.peer_addr(), viewer.local_addr());
            assert!(
                UdpSocket::bind(bound[1 - selected]).is_ok(),
                "unused socket must close before TLS handoff"
            );
            assert!(
                UdpSocket::bind(bound[selected]).is_err(),
                "the original selected socket stays owned"
            );
            let stream = viewer.connection_mut().open_uni_stream(&cx).unwrap();
            viewer
                .connection_mut()
                .write_stream(
                    &cx,
                    stream,
                    asupersync::bytes::Bytes::from_static(b"dual-stack authenticated stream"),
                    false,
                )
                .unwrap();
            viewer.flush(&cx).await.unwrap();
            let mut received = Vec::new();
            for _ in 0..20 {
                host.drive_io_once(&cx, Duration::from_millis(5))
                    .await
                    .unwrap();
                if let Ok(bytes) = host.connection_mut().read_stream(&cx, stream, 64) {
                    received.extend_from_slice(&bytes);
                    if received.len() == 31 {
                        break;
                    }
                }
            }
            assert_eq!(received, b"dual-stack authenticated stream");
            drop(host);
            drop(viewer);
            for address in bound {
                assert!(UdpSocket::bind(address).is_ok());
            }
        });
    }
}

// Routing-only Initial header, deliberately not a TLS fixture. The factory
// refuses immediately after recording which endpoint fairly won discovery.
fn candidate() -> Vec<u8> {
    let mut bytes = Vec::new();
    PacketHeader::Long(LongHeader {
        packet_type: LongPacketType::Initial,
        version: 1,
        dst_cid: id(b"candidate-dcid"),
        src_cid: id(b"candidate-scid"),
        token: vec![],
        payload_length: 1100,
        packet_number: 0,
        packet_number_len: 1,
    })
    .encode(&mut bytes)
    .unwrap();
    bytes.resize(1200, 0);
    bytes
}

#[test]
fn noisy_first_address_cannot_starve_a_waiting_other_family_candidate() {
    let _serial = serialized();
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let listener = Listener::bind_addresses(
            &cx,
            &addresses(),
            Configuration {
                max_initial_datagrams: 2,
                ..config()
            },
        )
        .await
        .unwrap();
        let bound = listener.local_addresses();
        let noise = UdpSocket::bind("127.0.0.1:0").unwrap();
        for _ in 0..8 {
            noise.send_to(&[0xff; 1200], bound[0]).unwrap();
        }
        let peer = UdpSocket::bind("[::1]:0").unwrap();
        peer.send_to(&candidate(), bound[1]).unwrap();
        let local = Cell::new(None);
        let result = listener
            .accept_with_local_identity(&cx, id(b"dual-host-cid"), |address, _, _| {
                local.set(Some(address));
                std::future::ready(Err(Error::IdentityUnavailable))
            })
            .unwrap()
            .await;
        assert!(matches!(result, Err(Error::IdentityUnavailable)));
        assert_eq!(local.get(), Some(bound[1]));
        for address in bound {
            assert!(UdpSocket::bind(address).is_ok());
        }
    });
}

#[test]
fn all_addresses_share_one_noise_budget_and_fixed_acquisition_deadline() {
    let _serial = serialized();
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let listener = Listener::bind_addresses(
            &cx,
            &addresses(),
            Configuration {
                max_initial_datagrams: 2,
                ..config()
            },
        )
        .await
        .unwrap();
        let bound = listener.local_addresses();
        // One bad datagram on each address exhausts the whole two-packet budget.
        // A mistaken per-address budget instead waits for more noise until timeout.
        for (local, remote) in addresses().into_iter().zip(&bound) {
            UdpSocket::bind(local)
                .unwrap()
                .send_to(&[0xff; 1200], remote)
                .unwrap();
        }
        let result = listener
            .accept(&cx, id(b"dual-host-cid"), |_| panic!("noise reached TLS"))
            .unwrap()
            .await;
        assert!(matches!(result, Err(Error::InitialBudget)));
        for address in bound {
            assert!(UdpSocket::bind(address).is_ok());
        }

        let listener = Listener::bind_addresses(
            &cx,
            &addresses(),
            Configuration {
                initial_timeout: Duration::from_millis(10),
                ..config()
            },
        )
        .await
        .unwrap();
        let bound = listener.local_addresses();
        let accepting = listener
            .accept(&cx, id(b"dual-host-cid"), |_| {
                panic!("expired acquisition reached TLS")
            })
            .unwrap();
        sleep(cx.now(), Duration::from_millis(20)).await;
        assert!(matches!(accepting.await, Err(Error::InitialTimeout)));
        for address in bound {
            assert!(UdpSocket::bind(address).is_ok());
        }
    });
}

#[test]
fn unpolled_accept_drop_and_invalid_or_partial_bind_release_every_socket() {
    let _serial = serialized();
    support::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let listener = Listener::bind_addresses(&cx, &addresses(), config())
            .await
            .unwrap();
        let bound = listener.local_addresses();
        drop(listener.accept(&cx, id(b"dual-host-cid"), server).unwrap());
        for &address in &bound {
            assert!(UdpSocket::bind(address).is_ok());
        }
        for addresses in [
            vec![],
            vec![bound[0]; 2],
            vec![bound[0]; accept::MAX_ADDRESSES + 1],
        ] {
            assert!(matches!(
                Listener::bind_addresses(&cx, &addresses, config()).await,
                Err(Error::Configuration)
            ));
        }
        let occupied = UdpSocket::bind(bound[1]).unwrap();
        assert!(matches!(
            Listener::bind_addresses(&cx, &bound, config()).await,
            Err(Error::Endpoint)
        ));
        assert!(
            UdpSocket::bind(bound[0]).is_ok(),
            "the first bind cannot leak when the second fails"
        );
        drop(occupied);
        assert!(UdpSocket::bind(bound[1]).is_ok());
    });
}
