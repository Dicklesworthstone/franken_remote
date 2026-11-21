//! Real-socket browser startup and origin-security regressions.
//!
//! Actual host bootstrap bytes are consumed by the actual Rust browser adapter.
//! The host truthfully refuses unavailable desktops without allocating a nonce,
//! binding, session, attachment ticket, media queue, or input authority. Policy-
//! issued test nonces exercise canonical ClientHello/refusal interoperability;
//! they do not represent installed-tailnet admission or real desktop support.

#![cfg(any(target_os = "linux", target_os = "macos", windows))]

use asupersync::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    runtime::RuntimeBuilder,
};
use fr_core::{
    ids::{HostBootId, OsSessionId, RemoteSessionId},
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_web::adapter::{BrowserAdapterError, BrowserAttempt, BrowserConnectionAdapter};
use fr_wire::{
    input::InputDelivery,
    negotiation::{Capability, Offer, Role, browser},
    refusal::{self, Reason},
};
use frd::broker::browser_security::{BrowserSessionRole, FIRST_MESSAGE_AUTH_TIMEOUT};
use frd::browser_ingress::{
    BrowserIngress, BrowserIngressConfig,
    websocket::{WsFrame, read_ws_frame, write_ws_frame},
};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

const EXPECTED_HOST: &str = "workstation.tailnet.ts.net";
const EXPECTED_PORT: u16 = 8443;
const EXPECTED_ORIGIN: &str = "https://workstation.tailnet.ts.net:8443";
const EXPECTED_HOST_HDR: &str = "workstation.tailnet.ts.net:8443";
const PEER_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 7));

fn test_ingress(max_credit_bytes: u64, max_credit_frames: u64) -> Arc<BrowserIngress> {
    let mut cfg = BrowserIngressConfig::https_tailnet(
        EXPECTED_HOST,
        EXPECTED_PORT,
        HostBootId::from_raw(11),
        OsSessionId::from_raw(22),
        ProtocolLimits::ABSOLUTE,
    );
    cfg.max_video_credit_bytes = max_credit_bytes;
    cfg.max_video_credit_frames = max_credit_frames;
    Arc::new(BrowserIngress::new(cfg))
}

async fn send_http_raw(
    ingress: &Arc<BrowserIngress>,
    peer_ip: IpAddr,
    sni: Option<&str>,
    raw_request: &[u8],
) -> String {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let ingress_srv = Arc::clone(ingress);
    let sni_owned = sni.map(str::to_owned);

    let (client_res, server_res) = futures_join(
        async move {
            let mut client = TcpStream::connect(addr).await.unwrap();
            client.write_all(raw_request).await.unwrap();
            client.flush().await.unwrap();
            let mut resp = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = client.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                resp.extend_from_slice(&buf[..n]);
            }
            String::from_utf8_lossy(&resp).into_owned()
        },
        async move {
            let (server, _) = listener.accept().await.unwrap();
            ingress_srv
                .serve_stream(server, peer_ip, sni_owned.as_deref(), || {
                    HostInstant::from_micros(10_000_000)
                })
                .await
                .unwrap();
        },
    )
    .await;
    let () = server_res;
    client_res
}

async fn futures_join<A, B>(
    fut_a: impl core::future::Future<Output = A>,
    fut_b: impl core::future::Future<Output = B>,
) -> (A, B) {
    use core::{pin::pin, task::Poll};
    let mut a = pin!(fut_a);
    let mut b = pin!(fut_b);
    let mut res_a = None;
    let mut res_b = None;
    core::future::poll_fn(|cx| {
        if res_a.is_none()
            && let Poll::Ready(v) = a.as_mut().poll(cx)
        {
            res_a = Some(v);
        }
        if res_b.is_none()
            && let Poll::Ready(v) = b.as_mut().poll(cx)
        {
            res_b = Some(v);
        }
        if res_a.is_some() && res_b.is_some() {
            Poll::Ready((res_a.take().unwrap(), res_b.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}

async fn read_http_head_from_stream<S: AsyncRead + Unpin>(stream: &mut S) -> String {
    let mut raw = Vec::new();
    let mut b = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut b).await.unwrap();
        assert_ne!(n, 0, "unexpected EOF reading HTTP response header");
        raw.push(b[0]);
    }
    String::from_utf8(raw).unwrap()
}

fn bootstrap_request(role: &str) -> String {
    let body = format!(r#"{{"role":"{role}"}}"#);
    format!(
        "POST /api/session/bootstrap HTTP/1.1\r\n\
         Host: {EXPECTED_HOST_HDR}\r\n\
         Origin: {EXPECTED_ORIGIN}\r\n\
         Sec-Fetch-Site: same-origin\r\n\
         Sec-Fetch-Mode: cors\r\n\
         Sec-Fetch-Dest: empty\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn upgrade_request(path: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {EXPECTED_HOST_HDR}\r\n\
         Origin: {EXPECTED_ORIGIN}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    )
}

fn issue_fixture_nonce(ingress: &BrowserIngress, role: BrowserSessionRole) -> [u8; 32] {
    *ingress
        .policy()
        .with_nonce_manager(|manager| {
            manager.issue_nonce(
                PEER_IP,
                RemoteSessionId::from_raw(101),
                role,
                HostInstant::from_micros(10_000_000),
            )
        })
        .unwrap()
        .unwrap()
        .as_bytes()
}

fn client_fixture(
    ingress: &BrowserIngress,
    role: &str,
) -> (BrowserConnectionAdapter, BrowserAttempt, Vec<u8>) {
    let nonce = issue_fixture_nonce(
        ingress,
        if role == "observer" {
            BrowserSessionRole::Observer
        } else {
            BrowserSessionRole::Controller
        },
    );
    // Explicit test-only available bootstrap, backed by a real policy nonce.
    // Production bootstrap remains unavailable and never issues this credential.
    let hex: String = nonce
        .iter()
        .flat_map(|byte| {
            let digits = b"0123456789abcdef";
            [
                char::from(digits[usize::from(byte >> 4)]),
                char::from(digits[usize::from(byte & 15)]),
            ]
        })
        .collect();
    let body = format!(
        r#"{{"nonce_hex":"{hex}","expires_in_ms":15000,"transport_profile":"wss_degraded_tcp_hol_dedicated","profile_id":3,"profile_version":0,"role":"{role}","desktop_available":true}}"#
    );
    let mut adapter = BrowserConnectionAdapter::new();
    let attempt = adapter.begin(role, 1_000_000).unwrap();
    adapter.accept_bootstrap(attempt, &body, 1_001_000).unwrap();
    let hello = adapter.client_hello(attempt, 1_002_000).unwrap();
    (adapter, attempt, hello)
}

async fn exchange_first_message(
    ingress: &Arc<BrowserIngress>,
    peer: IpAddr,
    message: &[u8],
    times: [u64; 2],
) -> WsFrame {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let ingress = Arc::clone(ingress);
    let (response, ()) = futures_join(
        async {
            let mut client = TcpStream::connect(addr).await.unwrap();
            client
                .write_all(upgrade_request("/ws/control").as_bytes())
                .await
                .unwrap();
            let head = read_http_head_from_stream(&mut client).await;
            assert!(head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
            assert!(head.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
            assert!(!head.to_ascii_lowercase().contains("permessage-deflate"));
            write_ws_frame(&mut client, 0x02, message, Some([1, 2, 3, 4]))
                .await
                .unwrap();
            let first = read_ws_frame(&mut client, browser::MAX_RECORD, false)
                .await
                .unwrap();
            if matches!(first, WsFrame::Binary(_)) {
                assert_eq!(
                    read_ws_frame(&mut client, 128, false).await.unwrap(),
                    WsFrame::Close
                );
            }
            let mut byte = [0];
            assert_eq!(client.read(&mut byte).await.unwrap(), 0);
            first
        },
        async {
            let (server, _) = listener.accept().await.unwrap();
            let counter = std::cell::Cell::new(0);
            ingress
                .serve_stream(server, peer, Some(EXPECTED_HOST), || {
                    let index = counter.get();
                    counter.set(1);
                    HostInstant::from_micros(times[index])
                })
                .await
                .unwrap();
        },
    )
    .await;
    response
}

fn refused(frame: &WsFrame) -> Reason {
    let WsFrame::Binary(bytes) = frame else {
        panic!("expected canonical refusal");
    };
    let refusal = refusal::decode(bytes, 0, browser::MAX_RECORD, InputDelivery::Reliable).unwrap();
    assert!(refusal.operation.is_none());
    assert!(refusal.stage.is_none());
    refusal.reason
}

#[test]
fn unavailable_bootstrap_reaches_the_real_browser_adapter_without_authority() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let ingress = test_ingress(4096, 4);
        let get_index = format!(
            "GET / HTTP/1.1\r\nHost: {EXPECTED_HOST_HDR}\r\nSec-Fetch-Dest: document\r\n\r\n"
        );
        let response =
            send_http_raw(&ingress, PEER_IP, Some(EXPECTED_HOST), get_index.as_bytes()).await;
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        for header in [
            "frame-ancestors 'none'",
            "X-Content-Type-Options: nosniff",
            "X-Frame-Options: DENY",
            "Referrer-Policy: no-referrer",
            "/wasm_client.js",
        ] {
            assert!(response.contains(header), "missing {header}");
        }
        for role in [
            "observer",
            "controller",
            "observer",
            "controller",
            "observer",
            "controller",
        ] {
            let response = send_http_raw(
                &ingress,
                PEER_IP,
                Some(EXPECTED_HOST),
                bootstrap_request(role).as_bytes(),
            )
            .await;
            assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
            assert!(response.contains("Cache-Control: no-cache, no-store"));
            assert!(response.contains("default-src 'none'; frame-ancestors 'none'"));
            let body = response.split_once("\r\n\r\n").unwrap().1;
            let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(parsed["desktop_available"], false);
            assert_eq!(parsed["profile_id"], browser::WSS_PROFILE);
            assert_eq!(parsed["profile_version"], browser::PROFILE_VERSION);
            assert_eq!(parsed["role"], role);
            for forbidden in [
                "nonce_hex",
                "session_id_hex",
                "host_boot_hex",
                "os_session_hex",
                "control_binding_id",
                "video_binding_id",
                "credit_epoch",
            ] {
                assert!(
                    parsed.get(forbidden).is_none(),
                    "unexpected authority field {forbidden}"
                );
            }
            let mut adapter = BrowserConnectionAdapter::new();
            let attempt = adapter.begin(role, 1_000_000).unwrap();
            assert_eq!(
                adapter.accept_bootstrap(attempt, body, 1_001_000),
                Err(BrowserAdapterError::DesktopUnavailable)
            );
            assert_eq!(
                adapter.snapshot().refusal_code,
                Some("browser_session_unavailable")
            );
            assert!(!adapter.snapshot().is_controlling);
            assert!(!adapter.snapshot().can_request_control);
        }
        assert_eq!(
            ingress
                .policy()
                .with_nonce_manager(|manager| manager.pending_count())
                .unwrap(),
            0
        );
        assert!(
            ingress
                .session_metrics(RemoteSessionId::from_raw(101))
                .unwrap()
                .is_none()
        );
    });
}

#[test]
fn canonical_hello_consumes_original_nonce_and_refuses_without_session_or_tickets() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        for role in ["observer", "controller"] {
            let ingress = test_ingress(4096, 4);
            let (mut adapter, attempt, hello) = client_fixture(&ingress, role);
            let response = exchange_first_message(&ingress, PEER_IP, &hello, [10_001_000; 2]).await;
            assert_eq!(refused(&response), Reason::HostUnavailable);
            let WsFrame::Binary(bytes) = response else {
                unreachable!()
            };
            assert_eq!(
                adapter.receive_control(attempt, &bytes, 1_003_000),
                Ok("host_unavailable")
            );
            assert!(!adapter.snapshot().is_controlling);
            assert!(!adapter.snapshot().can_request_control);
            assert!(
                ingress
                    .session_metrics(RemoteSessionId::from_raw(101))
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                ingress
                    .policy()
                    .with_nonce_manager(|manager| manager.pending_count())
                    .unwrap(),
                0
            );
            assert!(
                ingress
                    .issue_auxiliary_ticket_for_session(
                        RemoteSessionId::from_raw(101),
                        frd::broker::browser_security::AuxiliaryChannelRole::VideoStream,
                        HostInstant::from_micros(10_002_000)
                    )
                    .is_err()
            );
            let replay = exchange_first_message(&ingress, PEER_IP, &hello, [10_002_000; 2]).await;
            assert_eq!(refused(&replay), Reason::PermissionDenied);
        }
    });
}

#[test]
fn peer_role_and_deadline_refusals_do_not_consume_a_live_original_nonce() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let ingress = test_ingress(4096, 4);
        let (_, _, hello) = client_fixture(&ingress, "observer");
        let parsed = browser::decode_hello(&hello, browser::MAX_RECORD).unwrap();
        let mut changed = parsed.offer.clone();
        changed.role = Role::RequestControl;
        let changed = browser::Hello::new(changed, *parsed.nonce()).unwrap();
        let mut bytes = [0; browser::MAX_RECORD];
        let length = browser::encode_hello(&changed, browser::MAX_RECORD, &mut bytes).unwrap();
        assert_eq!(
            refused(
                &exchange_first_message(&ingress, PEER_IP, &bytes[..length], [10_001_000; 2]).await
            ),
            Reason::PermissionDenied
        );
        let other_peer = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 8));
        assert_eq!(
            refused(&exchange_first_message(&ingress, other_peer, &hello, [10_001_000; 2]).await),
            Reason::PermissionDenied
        );
        let expiry = 10_001_000 + FIRST_MESSAGE_AUTH_TIMEOUT.as_micros();
        assert_eq!(
            refused(&exchange_first_message(&ingress, PEER_IP, &hello, [10_001_000, expiry]).await),
            Reason::Expired
        );
        assert_eq!(
            refused(
                &exchange_first_message(&ingress, PEER_IP, &hello, [10_002_000, 10_001_000]).await
            ),
            Reason::Expired
        );
        assert_eq!(
            ingress
                .policy()
                .with_nonce_manager(|manager| manager.pending_count())
                .unwrap(),
            1
        );
        assert_eq!(
            refused(&exchange_first_message(&ingress, PEER_IP, &hello, [10_002_000; 2]).await),
            Reason::HostUnavailable
        );
    });
}

#[test]
fn legacy_malformed_profiles_and_required_capabilities_cannot_open_sessions() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let ingress = test_ingress(4096, 4);
        let (_, _, hello) = client_fixture(&ingress, "observer");
        let parsed = browser::decode_hello(&hello, browser::MAX_RECORD).unwrap();
        let mut legacy = b"FRBA".to_vec();
        legacy.extend_from_slice(parsed.nonce());
        assert_eq!(
            refused(&exchange_first_message(&ingress, PEER_IP, &legacy, [10_001_000; 2]).await),
            Reason::InvalidMessage
        );
        let mut trailing = hello.clone();
        trailing.push(0);
        assert_eq!(
            refused(&exchange_first_message(&ingress, PEER_IP, &trailing, [10_001_000; 2]).await),
            Reason::InvalidMessage
        );
        let mut wrong_profile = hello.clone();
        // FRD0 header (24), version count (4), one offered u16 version (2).
        wrong_profile[30] = 1;
        assert_eq!(
            refused(
                &exchange_first_message(&ingress, PEER_IP, &wrong_profile, [10_001_000; 2]).await
            ),
            Reason::UnsupportedProfile
        );
        let mut wrong_version = hello.clone();
        wrong_version[5] = 1;
        assert_eq!(
            refused(
                &exchange_first_message(&ingress, PEER_IP, &wrong_version, [10_001_000; 2]).await
            ),
            Reason::UnsupportedVersion
        );
        let mut terminal = [0; refusal::MAX_BYTES];
        let length = refusal::encode(
            refusal::Refused::connection(Reason::PermissionDenied),
            0,
            browser::MAX_RECORD,
            &mut terminal,
            InputDelivery::Reliable,
        )
        .unwrap();
        assert_eq!(
            exchange_first_message(&ingress, PEER_IP, &terminal[..length], [10_001_000; 2]).await,
            WsFrame::Close
        );
        assert_eq!(
            ingress
                .policy()
                .with_nonce_manager(|manager| manager.pending_count())
                .unwrap(),
            1
        );
        let hello = browser::Hello::new(
            Offer {
                capabilities: vec![Capability {
                    name: "unimplemented".into(),
                    version: 1,
                    required: true,
                }],
                ..parsed.offer.clone()
            },
            *parsed.nonce(),
        )
        .unwrap();
        let mut bytes = [0; browser::MAX_RECORD];
        let length = browser::encode_hello(&hello, browser::MAX_RECORD, &mut bytes).unwrap();
        assert_eq!(
            refused(
                &exchange_first_message(&ingress, PEER_IP, &bytes[..length], [10_001_000; 2]).await
            ),
            Reason::RequiredCapability
        );
        assert!(
            ingress
                .session_metrics(RemoteSessionId::from_raw(101))
                .unwrap()
                .is_none()
        );
    });
}

#[test]
fn adversarial_origin_host_smuggling_and_ticket_attacks_are_refused() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let ingress = test_ingress(4096, 4);

        // 1. GET /api/session/bootstrap -> 405 Method Not Allowed (never issues nonce on GET).
        let r = send_http_raw(
            &ingress,
            PEER_IP,
            Some(EXPECTED_HOST),
            format!(
                "GET /api/session/bootstrap HTTP/1.1\r\nHost: {EXPECTED_HOST_HDR}\r\nOrigin: {EXPECTED_ORIGIN}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 405"), "{r}");

        // 2. Missing Origin, Origin: null, Origin: *, and cross-origin on bootstrap POST -> 403.
        for bad_origin in [
            None,
            Some("null"),
            Some("*"),
            Some("https://evil.example:8443"),
            Some("http://workstation.tailnet.ts.net:8443"),
        ] {
            let mut req = format!(
                "POST /api/session/bootstrap HTTP/1.1\r\nHost: {EXPECTED_HOST_HDR}\r\nContent-Length: 0\r\n"
            );
            if let Some(o) = bad_origin {
                req.push_str(&format!("Origin: {o}\r\n"));
            }
            req.push_str("\r\n");
            let r = send_http_raw(&ingress, PEER_IP, Some(EXPECTED_HOST), req.as_bytes()).await;
            assert!(
                r.starts_with("HTTP/1.1 403"),
                "bad origin {bad_origin:?} was not refused: {r}"
            );
        }

        // 3. DNS rebinding Host mismatch and TLS SNI mismatch -> 403 even on GET /.
        let rebind = send_http_raw(
            &ingress,
            PEER_IP,
            Some(EXPECTED_HOST),
            b"GET / HTTP/1.1\r\nHost: attacker.evil:8443\r\n\r\n",
        )
        .await;
        assert!(rebind.starts_with("HTTP/1.1 403"), "{rebind}");
        let bad_sni = send_http_raw(
            &ingress,
            PEER_IP,
            Some("wrong.tailnet.ts.net"),
            format!("GET / HTTP/1.1\r\nHost: {EXPECTED_HOST_HDR}\r\n\r\n").as_bytes(),
        )
        .await;
        assert!(bad_sni.starts_with("HTTP/1.1 403"), "{bad_sni}");

        // 4. Iframe embedding and cross-site Fetch Metadata -> 403.
        let iframe = send_http_raw(
            &ingress,
            PEER_IP,
            Some(EXPECTED_HOST),
            format!(
                "GET / HTTP/1.1\r\nHost: {EXPECTED_HOST_HDR}\r\nSec-Fetch-Dest: iframe\r\n\r\n"
            )
            .as_bytes(),
        )
        .await;
        assert!(iframe.starts_with("HTTP/1.1 403"), "{iframe}");

        let cross_site = send_http_raw(
            &ingress,
            PEER_IP,
            Some(EXPECTED_HOST),
            format!(
                "POST /api/session/bootstrap HTTP/1.1\r\n\
                 Host: {EXPECTED_HOST_HDR}\r\n\
                 Origin: {EXPECTED_ORIGIN}\r\n\
                 Sec-Fetch-Site: cross-site\r\n\
                 Content-Length: 0\r\n\r\n"
            )
            .as_bytes(),
        )
        .await;
        assert!(cross_site.starts_with("HTTP/1.1 403"), "{cross_site}");

        // 5. Bearer token/nonce/ticket in query string -> 403.
        for bad_uri in ["/?nonce=deadbeef", "/ws/control?ticket=1234", "/?token=abc"] {
            let r = send_http_raw(
                &ingress,
                PEER_IP,
                Some(EXPECTED_HOST),
                format!("GET {bad_uri} HTTP/1.1\r\nHost: {EXPECTED_HOST_HDR}\r\n\r\n").as_bytes(),
            )
            .await;
            assert!(r.starts_with("HTTP/1.1 403"), "{bad_uri}: {r}");
        }

        // 6. HTTP request smuggling (Transfer-Encoding or duplicate Content-Length) -> 400.
        let te = send_http_raw(
            &ingress,
            PEER_IP,
            Some(EXPECTED_HOST),
            format!(
                "POST /api/session/bootstrap HTTP/1.1\r\n\
                 Host: {EXPECTED_HOST_HDR}\r\n\
                 Origin: {EXPECTED_ORIGIN}\r\n\
                 Transfer-Encoding: chunked\r\n\r\n"
            )
            .as_bytes(),
        )
        .await;
        assert!(te.starts_with("HTTP/1.1 400"), "{te}");

        let dup_cl = send_http_raw(
            &ingress,
            PEER_IP,
            Some(EXPECTED_HOST),
            format!(
                "POST /api/session/bootstrap HTTP/1.1\r\n\
                 Host: {EXPECTED_HOST_HDR}\r\n\
                 Origin: {EXPECTED_ORIGIN}\r\n\
                 Content-Length: 0\r\n\
                 Content-Length: 2\r\n\r\n"
            )
            .as_bytes(),
        )
        .await;
        assert!(dup_cl.starts_with("HTTP/1.1 400"), "{dup_cl}");

        // 7. Bare session ID (no ticket) on /ws/video and text WebSocket frame on /ws/control -> Close.
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let ingress_srv = Arc::clone(&ingress);
        let (_, _) = futures_join(
            async move {
                let (s, _) = listener.accept().await.unwrap();
                ingress_srv
                    .serve_stream(s, PEER_IP, Some(EXPECTED_HOST), || {
                        HostInstant::from_micros(20_000_000)
                    })
                    .await
                    .unwrap();
            },
            async move {
                let mut client = TcpStream::connect(addr).await.unwrap();
                let up = format!(
                    "GET /ws/video HTTP/1.1\r\n\
                     Host: {EXPECTED_HOST_HDR}\r\n\
                     Origin: {EXPECTED_ORIGIN}\r\n\
                     Upgrade: websocket\r\n\
                     Connection: Upgrade\r\n\
                     Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                     Sec-WebSocket-Version: 13\r\n\r\n"
                );
                client.write_all(up.as_bytes()).await.unwrap();
                client.flush().await.unwrap();
                let head = read_http_head_from_stream(&mut client).await;
                assert!(head.starts_with("HTTP/1.1 101"));
                // An unavailable desktop never accepts any auxiliary attachment,
                // even before the caller presents a session ID or bearer ticket.
                let frame = read_ws_frame(&mut client, 64, false).await.unwrap();
                assert_eq!(frame, WsFrame::Close);
            },
        )
        .await;
    });
}

#[test]
fn invalid_bootstrap_roles_and_oversized_bodies_never_issue_credentials() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let ingress = test_ingress(4096, 4);
        for body in [r#"{}"#, r#"{"role":null}"#, r#"{"role":"other"}"#, r#"{"role":"observer","role":"controller"}"#, r#"{"role":"observer","session_id_hex":"1"}"#] {
            let request = format!("POST /api/session/bootstrap HTTP/1.1\r\nHost: {EXPECTED_HOST_HDR}\r\nOrigin: {EXPECTED_ORIGIN}\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            let response = send_http_raw(&ingress, PEER_IP, Some(EXPECTED_HOST), request.as_bytes()).await;
            assert!(response.starts_with("HTTP/1.1 400 "));
            assert!(!response.contains("nonce_hex"));
        }
        let request = format!("POST /api/session/bootstrap HTTP/1.1\r\nHost: {EXPECTED_HOST_HDR}\r\nOrigin: {EXPECTED_ORIGIN}\r\nContent-Length: 4097\r\n\r\n");
        let response = send_http_raw(&ingress, PEER_IP, Some(EXPECTED_HOST), request.as_bytes()).await;
        assert!(response.starts_with("HTTP/1.1 413 "));
        assert_eq!(ingress.policy().with_nonce_manager(|manager| manager.pending_count()).unwrap(), 0);
    });
}

#[test]
fn text_compressed_unmasked_fragmented_and_oversized_first_frames_close() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let ingress = test_ingress(4096, 4);
        for raw in [
            vec![0x81, 0x80],             // text, refused before mask/payload
            vec![0xc2, 0x80],             // compression, refused before mask/payload
            vec![0x82, 0],                // unmasked
            vec![0x02, 0x80],             // fragmented, refused before mask/payload
            vec![0x82, 0xfe, 0x10, 0x01], // 4097 B rejected before mask/payload
        ] {
            let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let ((), ()) = futures_join(
                async {
                    let mut stream = TcpStream::connect(addr).await.unwrap();
                    stream
                        .write_all(upgrade_request("/ws/control").as_bytes())
                        .await
                        .unwrap();
                    assert!(
                        read_http_head_from_stream(&mut stream)
                            .await
                            .starts_with("HTTP/1.1 101 ")
                    );
                    stream.write_all(&raw).await.unwrap();
                    assert_eq!(
                        read_ws_frame(&mut stream, 128, false).await.unwrap(),
                        WsFrame::Close
                    );
                },
                async {
                    let (stream, _) = listener.accept().await.unwrap();
                    ingress
                        .serve_stream(stream, PEER_IP, Some(EXPECTED_HOST), || {
                            HostInstant::from_micros(10_000_000)
                        })
                        .await
                        .unwrap();
                },
            )
            .await;
        }
        assert!(
            ingress
                .session_metrics(RemoteSessionId::from_raw(101))
                .unwrap()
                .is_none()
        );
    });
}

#[test]
fn a_silent_first_message_expires_even_when_the_supplied_clock_does_not_advance() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut config = BrowserIngressConfig::https_tailnet(
            EXPECTED_HOST,
            EXPECTED_PORT,
            HostBootId::from_raw(11),
            OsSessionId::from_raw(22),
            ProtocolLimits::with_overrides(fr_core::limits::LimitOverrides {
                max_handshake_duration_ms: Some(1_000),
                ..Default::default()
            })
            .unwrap(),
        );
        config.max_video_credit_bytes = 4096;
        let ingress = Arc::new(BrowserIngress::new(config));
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let ((), ()) = futures_join(
            async {
                let mut stream = TcpStream::connect(addr).await.unwrap();
                stream
                    .write_all(upgrade_request("/ws/control").as_bytes())
                    .await
                    .unwrap();
                assert!(
                    read_http_head_from_stream(&mut stream)
                        .await
                        .starts_with("HTTP/1.1 101 ")
                );
                // Deliberately send no authentication bytes. Expiry must wake
                // independently of read completion or this fixed test clock.
                let mut byte = [0];
                assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
            },
            async {
                let (stream, _) = listener.accept().await.unwrap();
                let error = ingress
                    .serve_stream(stream, PEER_IP, Some(EXPECTED_HOST), || {
                        HostInstant::from_micros(10_000_000)
                    })
                    .await
                    .unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
            },
        )
        .await;
    });
}

#[test]
fn incomplete_http_headers_and_bootstrap_bodies_expire_without_allocating_nonce() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let limits = ProtocolLimits::with_overrides(fr_core::limits::LimitOverrides {
            max_handshake_duration_ms: Some(1_000),
            ..Default::default()
        }).unwrap();
        let ingress = Arc::new(BrowserIngress::new(BrowserIngressConfig::https_tailnet(
            EXPECTED_HOST, EXPECTED_PORT, HostBootId::from_raw(11), OsSessionId::from_raw(22), limits,
        )));
        for request in [
            format!("POST /api/session/bootstrap HTTP/1.1\r\nHost: {EXPECTED_HOST_HDR}\r\n"),
            format!("POST /api/session/bootstrap HTTP/1.1\r\nHost: {EXPECTED_HOST_HDR}\r\nOrigin: {EXPECTED_ORIGIN}\r\nContent-Length: 19\r\n\r\n{{\"role\":"),
        ] {
            let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
            let addr = listener.local_addr().unwrap();
            let ((), ()) = futures_join(
                async {
                    let mut stream = TcpStream::connect(addr).await.unwrap();
                    stream.write_all(request.as_bytes()).await.unwrap();
                    let mut byte = [0];
                    assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
                },
                async {
                    let (stream, _) = listener.accept().await.unwrap();
                    let error = ingress.serve_stream(stream, PEER_IP, Some(EXPECTED_HOST), || HostInstant::from_micros(10_000_000)).await.unwrap_err();
                    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
                },
            ).await;
        }
        assert_eq!(ingress.policy().with_nonce_manager(|manager| manager.pending_count()).unwrap(), 0);
    });
}

#[path = "browser_ingress/tls.rs"]
mod tls;
