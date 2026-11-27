//! Genuine TCP/TLS handshakes against the production installed-credential owner.
//! The local daemon, identity metadata and CA are explicit test fixtures.
use super::*;
use asupersync::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    tls::TlsConnector,
};
use std::future::{Future, poll_fn};
use std::pin::pin;
use std::task::Poll;

async fn sockets() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (client, server) = both(TcpStream::connect(address), listener.accept()).await;
    (server.unwrap().0, client.unwrap())
}

fn connector(protocols: Vec<Vec<u8>>, version: u16, sni: bool) -> TlsConnector {
    let builder = TlsConnector::builder()
        .with_strict_ca_validation()
        .add_root_certificates(
            Certificate::from_pem(&std::fs::read(pki().join("ca.pem")).unwrap()).unwrap(),
        )
        .alpn_protocols(protocols)
        .min_protocol_version(version.into())
        .max_protocol_version(version.into())
        .handshake_timeout(Duration::from_secs(2));
    (if sni { builder } else { builder.disable_sni() })
        .build()
        .unwrap()
}

fn leaf(name: &str) -> Vec<u8> {
    Certificate::from_pem(&std::fs::read(pki().join(format!("{name}.pem"))).unwrap()).unwrap()[0]
        .as_der()
        .to_vec()
}

#[test]
fn actual_https_uses_the_installed_and_rotated_leaf_for_http11_tls13() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let identity = fixture.identity(&cx).await;
        for generation in 1..=2 {
            let name = if generation == 1 { "one" } else { "two" };
            if generation == 2 {
                *fixture.reply.lock().unwrap() = Reply::pair(name);
                due(&identity, &cx);
                identity.refresh(&cx).await.unwrap();
            }
            let node = fixture.api.node_identity(&cx).await.unwrap();
            let client = connector(vec![b"h2".to_vec(), HTTPS_ALPN.to_vec()], 0x0304, true);
            let (server, peer) = sockets().await;
            let (server, peer) = both(
                identity.https_accept(&cx, &node, server),
                client.connect(HOST, peer),
            )
            .await;
            let (mut server, mut peer) = (server.unwrap(), peer.unwrap());
            assert_eq!(server.alpn_protocol(), Some(HTTPS_ALPN));
            assert_eq!(peer.alpn_protocol(), Some(HTTPS_ALPN));
            assert_eq!(server.protocol_version(), Some(0x0304u16.into()));
            assert_eq!(peer.protocol_version(), Some(0x0304u16.into()));
            assert_eq!(server.sni_hostname(), Some(HOST));
            assert_eq!(peer.peer_leaf_certificate_der(), Some(leaf(name)));
            let request = b"GET / HTTP/1.1\r\nHost: host.fixture.ts.net\r\n\r\n";
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            timeout(
                cx.now(),
                Duration::from_secs(2),
                both(
                    async {
                        let mut bytes = vec![0; request.len()];
                        server.read_exact(&mut bytes).await.unwrap();
                        assert_eq!(bytes, request);
                        server.write_all(response).await.unwrap();
                        server.flush().await.unwrap();
                    },
                    async {
                        peer.write_all(request).await.unwrap();
                        peer.flush().await.unwrap();
                        let mut bytes = vec![0; response.len()];
                        peer.read_exact(&mut bytes).await.unwrap();
                        assert_eq!(bytes, response);
                    },
                ),
            )
            .await
            .unwrap();
            // Neither the HTTPS owner nor the unchanged native QUIC owner
            // admits replayable early data.
            let state = identity.state().unwrap();
            let pair = state.active.as_ref().unwrap();
            assert_eq!(pair.https.config().max_early_data_size, 0);
            assert_eq!(pair.acceptor.config().max_early_data_size, 0);
            assert_eq!(pair.acceptor.config().alpn_protocols, vec![ALPN.to_vec()]);
            assert_eq!(state.status.generation, generation);
        }
    });
}

#[test]
fn actual_https_refuses_other_alpn_tls12_and_missing_sni() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let identity = fixture.identity(&cx).await;
        for case in 0..5 {
            let protocols = match case {
                0 => vec![b"h2".to_vec()],
                1 => vec![ALPN.to_vec()],
                2 => vec![],
                _ => vec![HTTPS_ALPN.to_vec()],
            };
            let client = connector(
                protocols,
                if case == 3 { 0x0303 } else { 0x0304 },
                case != 4,
            );
            let node = fixture.api.node_identity(&cx).await.unwrap();
            let (server, peer) = sockets().await;
            let (accepted, _) = both(
                identity.https_accept(&cx, &node, server),
                client.connect(HOST, peer),
            )
            .await;
            assert_eq!(accepted.err(), Some(Error::HttpsHandshake), "case {case}");
            assert!(identity.status(&cx).is_ok());
        }
    });
}

fn alias_pair() -> Vec<u8> {
    static PAIR: OnceLock<Vec<u8>> = OnceLock::new();
    PAIR.get_or_init(|| {
        std::fs::write(
            pki().join("https-alias.extensions"),
            format!("subjectAltName=DNS:{HOST},DNS:alias.fixture.ts.net\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n"),
        ).unwrap();
        openssl(pki(), &[
            "x509", "-req", "-sha256", "-in", "one.csr", "-CA", "ca.pem",
            "-CAkey", "ca.key", "-set_serial", "990", "-days", "2",
            "-extfile", "https-alias.extensions", "-out", "https-alias.pem",
        ]);
        let mut pair = std::fs::read(pki().join("one.key")).unwrap();
        pair.extend(std::fs::read(pki().join("https-alias.pem")).unwrap());
        pair
    }).clone()
}

#[test]
fn actual_https_requires_the_canonical_node_sni_even_with_a_valid_alias_certificate() {
    let fixture = Fixture::new();
    fixture.reply.lock().unwrap().body = alias_pair();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let identity = fixture.identity(&cx).await;
        let node = fixture.api.node_identity(&cx).await.unwrap();
        let client = connector(vec![HTTPS_ALPN.to_vec()], 0x0304, true);
        let (server, peer) = sockets().await;
        let (accepted, connected) = both(
            identity.https_accept(&cx, &node, server),
            client.connect("alias.fixture.ts.net", peer),
        )
        .await;
        // The client validates both the genuine CA and alias SAN. Refusal
        // therefore comes from the host's exact installed-node SNI gate.
        assert!(connected.is_ok());
        assert_eq!(accepted.err(), Some(Error::HttpsHandshake));
    });
}

#[test]
fn https_refuses_foreign_expired_changed_and_stopped_node_lifetimes() {
    for case in 0..4 {
        let fixture = Fixture::new();
        let foreign = Fixture::new();
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let identity = fixture.identity(&cx).await;
            let mut node = if case == 0 {
                foreign.api.node_identity(&cx).await.unwrap()
            } else {
                fixture.api.node_identity(&cx).await.unwrap()
            };
            let expected = match case {
                0 => Error::IdentityMismatch,
                1 => {
                    node.expires_us = now(&cx).unwrap();
                    Error::Expired
                }
                2 => {
                    fixture.status.lock().unwrap()["Self"]["PublicKey"] =
                        json!(format!("nodekey:{}", "9".repeat(64)));
                    node = fixture.api.node_identity(&cx).await.unwrap();
                    Error::IdentityChanged
                }
                _ => {
                    identity.stop();
                    Error::Revoked
                }
            };
            let (server, mut peer) = sockets().await;
            assert_eq!(
                identity.https_accept(&cx, &node, server).await.err(),
                Some(expected)
            );
            let mut byte = [0];
            assert_eq!(
                timeout(cx.now(), Duration::from_secs(1), peer.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0,
            );
        });
    }
}

#[test]
fn stopping_the_original_credential_closes_an_already_waiting_https_handshake() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let identity = fixture.identity(&cx).await;
        let node = fixture.api.node_identity(&cx).await.unwrap();
        let (server, mut peer) = sockets().await;
        let mut accepted = pin!(identity.https_accept(&cx, &node, server));
        poll_fn(|task| {
            assert!(accepted.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        identity.stop();
        assert_eq!(
            timeout(cx.now(), Duration::from_secs(1), accepted)
                .await
                .unwrap()
                .err(),
            Some(Error::Revoked),
        );
        let mut byte = [0];
        assert_eq!(
            timeout(cx.now(), Duration::from_secs(1), peer.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0,
        );
    });
}

#[test]
fn a_waiting_https_handshake_cannot_borrow_the_rotated_leafs_lifetime() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let identity = fixture.identity(&cx).await;
        // Advance the owner's conservative validity metadata in this fixture;
        // genuine DER validation and actual TCP/TLS ownership remain enabled.
        let original_expiry = wall_now().unwrap() + 10_000_000;
        {
            let mut state = identity.state().unwrap();
            Arc::get_mut(state.active.as_mut().unwrap())
                .unwrap()
                .not_after_wall_us = original_expiry;
            state.status.not_after_wall_us = Some(original_expiry);
        }
        let node = fixture.api.node_identity(&cx).await.unwrap();
        let (server, _peer) = sockets().await;
        let mut accepted = pin!(identity.https_accept(&cx, &node, server));
        poll_fn(|task| {
            assert!(accepted.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        *fixture.reply.lock().unwrap() = Reply::pair("two");
        due(&identity, &cx);
        identity.refresh(&cx).await.unwrap();
        identity.state().unwrap().last_wall_us = original_expiry;
        assert_eq!(
            timeout(cx.now(), Duration::from_secs(1), accepted)
                .await
                .unwrap()
                .err(),
            Some(Error::Expired),
        );
        // The replacement remains valid for a newly admitted handshake.
        assert!(identity.status(&cx).is_ok());
    });
}
