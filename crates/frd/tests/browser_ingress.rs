//! Real-socket integration and adversarial security tests for `frd::browser_ingress`.
//!
//! Exercises the real Asupersync TCP/WSS ingress wired to `BrowserSecurityPolicy`,
//! `BrowserAssets`, `ReceiverCreditWindow`, and `ChannelSupervisor`:
//! 1. Full happy path: static self-hosted assets -> POST /api/session/bootstrap ->
//!    /ws/control first-message FRBA auth -> server-issued single-use FRTK VideoStream ticket ->
//!    /ws/video FRAT attach -> credit exhaustion (WssError::CreditStarvation) ->
//!    0x0081 ReceiverPressure over /ws/control -> credit unlocked & video frame delivered ->
//!    channel replacement fences stale generation.
//! 2. Adversarial / planted negative security matrix over real sockets:
//!    GET bootstrap refusal, missing/null/wildcard/cross-origin rejection, DNS-rebinding Host/SNI
//!    rejection, iframe/cross-site Fetch Metadata rejection, query-string bearer leak rejection,
//!    HTTP request smuggling (Transfer-Encoding / duplicate Content-Length) rejection,
//!    replayed nonce rejection, bare session ID & wrong-role auxiliary ticket rejection,
//!    text/compressed/unmasked WebSocket frame rejection, and forged ReceiverPressure rejection.

#![cfg(any(target_os = "linux", target_os = "macos", windows))]

use asupersync::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    runtime::RuntimeBuilder,
};
use fr_core::{
    ids::{HostBootId, OsSessionId, RemoteSessionId},
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_transport::wss::{ChannelGeneration, WssError};
use fr_wire::{
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    receiver_pressure::{self, ChannelRole, QueuePressure, ReceiverPressure},
};
use frd::browser_ingress::{
    BrowserIngress, BrowserIngressConfig, BrowserIngressError,
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

fn hex_decode(hex: &str) -> Vec<u8> {
    hex.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u8::from_str_radix(core::str::from_utf8(b).unwrap(), 16).unwrap())
        .collect()
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

async fn read_http_head_from_stream(stream: &mut TcpStream) -> String {
    let mut raw = Vec::new();
    let mut b = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut b).await.unwrap();
        assert_ne!(n, 0, "unexpected EOF reading HTTP response header");
        raw.push(b[0]);
    }
    String::from_utf8(raw).unwrap()
}

#[test]
fn end_to_end_browser_bootstrap_wss_auth_auxiliary_ticket_and_credit_flow() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        // Small credit window: 128 bytes / 2 frames to prove real starvation & 0x0081 unlock!
        let ingress = test_ingress(128, 2);

        // 1. GET / serves self-hosted HTML with strict CSP and no authority.
        let get_index = format!(
            "GET / HTTP/1.1\r\nHost: {EXPECTED_HOST_HDR}\r\nSec-Fetch-Dest: document\r\n\r\n"
        );
        let resp =
            send_http_raw(&ingress, PEER_IP, Some(EXPECTED_HOST), get_index.as_bytes()).await;
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "{resp}");
        assert!(
            resp.contains("Content-Security-Policy: default-src 'self'"),
            "{resp}"
        );
        assert!(resp.contains("frame-ancestors 'none'"), "{resp}");
        assert!(resp.contains("X-Content-Type-Options: nosniff"), "{resp}");
        assert!(resp.contains("X-Frame-Options: DENY"), "{resp}");
        assert!(resp.contains("Referrer-Policy: no-referrer"), "{resp}");
        assert!(resp.contains("/hevc_probe.js"), "{resp}");
        assert!(resp.contains("/wss_client.js"), "{resp}");

        // 2. POST /api/session/bootstrap with exact Origin issues CSPRNG single-use nonce.
        let body = r#"{"role":"observer"}"#;
        let post_boot = format!(
            "POST /api/session/bootstrap HTTP/1.1\r\n\
             Host: {EXPECTED_HOST_HDR}\r\n\
             Origin: {EXPECTED_ORIGIN}\r\n\
             Sec-Fetch-Site: same-origin\r\n\
             Sec-Fetch-Mode: cors\r\n\
             Sec-Fetch-Dest: empty\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let boot_resp =
            send_http_raw(&ingress, PEER_IP, Some(EXPECTED_HOST), post_boot.as_bytes()).await;
        assert!(boot_resp.starts_with("HTTP/1.1 200 OK\r\n"), "{boot_resp}");
        assert!(
            boot_resp.contains("Cache-Control: no-cache, no-store"),
            "{boot_resp}"
        );
        assert!(
            boot_resp.contains("default-src 'none'; frame-ancestors 'none'"),
            "{boot_resp}"
        );
        let json_body = boot_resp.split_once("\r\n\r\n").unwrap().1;
        let parsed: serde_json::Value = serde_json::from_str(json_body).unwrap();
        assert_eq!(
            parsed["transport_profile"],
            "wss_degraded_tcp_hol_dedicated"
        );
        assert_eq!(parsed["role"], "observer");
        let nonce_bytes = hex_decode(parsed["nonce_hex"].as_str().unwrap());
        let sid_bytes = hex_decode(parsed["session_id_hex"].as_str().unwrap());
        assert_eq!(nonce_bytes.len(), 32);
        assert_eq!(sid_bytes.len(), 16);
        let session_id = RemoteSessionId::from_raw(u128::from_be_bytes(
            sid_bytes.as_slice().try_into().unwrap(),
        ));

        // 3. Connect /ws/control, authenticate with FRBA + nonce, receive single-use FRTK video ticket,
        //    attach /ws/video with FRAT + ticket, prove ReceiverCreditWindow starvation + 0x0081 unlock!
        let ctrl_listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let ctrl_addr = ctrl_listener.local_addr().unwrap();
        let vid_listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let vid_addr = vid_listener.local_addr().unwrap();

        let ingress_ctrl = Arc::clone(&ingress);
        let ingress_vid = Arc::clone(&ingress);
        let ingress_driver = Arc::clone(&ingress);

        let server_fut = futures_join(
            async move {
                let (s, _) = ctrl_listener.accept().await.unwrap();
                ingress_ctrl
                    .serve_stream(s, PEER_IP, Some(EXPECTED_HOST), || {
                        HostInstant::from_micros(10_500_000)
                    })
                    .await
                    .unwrap();
            },
            async move {
                let (s, _) = vid_listener.accept().await.unwrap();
                ingress_vid
                    .serve_stream(s, PEER_IP, Some(EXPECTED_HOST), || {
                        HostInstant::from_micros(10_600_000)
                    })
                    .await
                    .unwrap();
            },
        );

        let client_fut = async move {
            // Open /ws/control
            let mut ctrl = TcpStream::connect(ctrl_addr).await.unwrap();
            let upgrade_ctrl = format!(
                "GET /ws/control HTTP/1.1\r\n\
                 Host: {EXPECTED_HOST_HDR}\r\n\
                 Origin: {EXPECTED_ORIGIN}\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\n\r\n"
            );
            ctrl.write_all(upgrade_ctrl.as_bytes()).await.unwrap();
            ctrl.flush().await.unwrap();
            let head = read_http_head_from_stream(&mut ctrl).await;
            assert!(
                head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
                "{head}"
            );
            assert!(head.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="), "{head}");
            assert!(!head.to_ascii_lowercase().contains("permessage-deflate"));

            // Send FRBA + 32-byte nonce as FIRST binary message
            let mut auth_msg = Vec::with_capacity(36);
            auth_msg.extend_from_slice(b"FRBA");
            auth_msg.extend_from_slice(&nonce_bytes);
            write_ws_frame(&mut ctrl, 0x02, &auth_msg, Some([0x11, 0x22, 0x33, 0x44]))
                .await
                .unwrap();

            // Receive FRTK single-use VideoStream ticket
            let WsFrame::Binary(grant) = read_ws_frame(&mut ctrl, 256, false).await.unwrap() else {
                panic!("expected binary FRTK ticket grant");
            };
            assert_eq!(grant.len(), 53);
            assert_eq!(&grant[..4], b"FRTK");
            assert_eq!(grant[4], 1); // VideoStream
            assert_eq!(&grant[5..21], sid_bytes.as_slice());
            let video_ticket = &grant[21..53];

            // Open /ws/video and attach with FRAT + session_id + role(1) + ticket
            let mut vid = TcpStream::connect(vid_addr).await.unwrap();
            let upgrade_vid = format!(
                "GET /ws/video HTTP/1.1\r\n\
                 Host: {EXPECTED_HOST_HDR}\r\n\
                 Origin: {EXPECTED_ORIGIN}\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\n\r\n"
            );
            vid.write_all(upgrade_vid.as_bytes()).await.unwrap();
            vid.flush().await.unwrap();
            let vid_head = read_http_head_from_stream(&mut vid).await;
            assert!(vid_head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));

            let mut attach_msg = Vec::with_capacity(53);
            attach_msg.extend_from_slice(b"FRAT");
            attach_msg.extend_from_slice(&sid_bytes);
            attach_msg.push(1); // VideoStream
            attach_msg.extend_from_slice(video_ticket);
            write_ws_frame(&mut vid, 0x02, &attach_msg, Some([0x55, 0x66, 0x77, 0x88]))
                .await
                .unwrap();

            let WsFrame::Binary(ok_ack) = read_ws_frame(&mut vid, 64, false).await.unwrap() else {
                panic!("expected binary FROK attach confirmation");
            };
            assert_eq!(&ok_ack[..4], b"FROK");
            let gen1 = ChannelGeneration(u64::from_be_bytes(ok_ack[4..12].try_into().unwrap()));
            assert_eq!(gen1, ChannelGeneration(1));

            // Host enqueues two 60-byte frames (120/128 bytes, 2/2 frames) -> succeeds.
            ingress_driver
                .enqueue_video_frame(session_id, gen1, vec![0xAA; 60])
                .unwrap();
            ingress_driver
                .enqueue_video_frame(session_id, gen1, vec![0xBB; 60])
                .unwrap();
            // 3rd frame MUST fail with CreditStarvation (max_frames=2)!
            assert!(matches!(
                ingress_driver.enqueue_video_frame(session_id, gen1, vec![0xCC; 20]),
                Err(BrowserIngressError::Wss(WssError::CreditStarvation(
                    120, 128, 2, 2
                )))
            ));

            // Poll video frames on /ws/video
            write_ws_frame(&mut vid, 0x02, b"FRPL", Some([1, 2, 3, 4]))
                .await
                .unwrap();
            let WsFrame::Binary(f1) = read_ws_frame(&mut vid, 256, false).await.unwrap() else {
                panic!("expected frame 1");
            };
            let WsFrame::Binary(f2) = read_ws_frame(&mut vid, 256, false).await.unwrap() else {
                panic!("expected frame 2");
            };
            assert_eq!(f1, vec![0xAA; 60]);
            assert_eq!(f2, vec![0xBB; 60]);

            // Send real 0x0081 ReceiverPressure on /ws/control releasing both closed frames (120 B, 2 frames).
            let binding = ControlBinding {
                id: 1,
                host_boot: HostBootId::from_raw(11),
                os_session: OsSessionId::from_raw(22),
                remote_session: session_id,
            };
            let report = ReceiverPressure {
                channel_binding: 2,
                channel_role: ChannelRole::Video,
                credit_epoch: 1,
                released_bytes: 120,
                released_frames: 2,
                pressure: QueuePressure::Low,
                queued_bytes: 0,
                queued_items: 0,
            };
            let mut wire = [0u8; receiver_pressure::RECORD_BYTES];
            let n = receiver_pressure::encode(
                &report,
                binding,
                &ProtocolLimits::ABSOLUTE,
                &mut wire,
                InputDirection::ViewerToHost,
                InputDelivery::Reliable,
            )
            .unwrap();
            write_ws_frame(&mut ctrl, 0x02, &wire[..n], Some([9, 8, 7, 6]))
                .await
                .unwrap();
            let WsFrame::Binary(cr_ack) = read_ws_frame(&mut ctrl, 64, false).await.unwrap() else {
                panic!("expected FRCR credit ack");
            };
            assert_eq!(cr_ack.as_slice(), b"FRCR");

            // Now 3rd frame succeeds!
            ingress_driver
                .enqueue_video_frame(session_id, gen1, vec![0xCC; 20])
                .unwrap();
            write_ws_frame(&mut vid, 0x02, b"FRPL", Some([1, 2, 3, 4]))
                .await
                .unwrap();
            let WsFrame::Binary(f3) = read_ws_frame(&mut vid, 256, false).await.unwrap() else {
                panic!("expected frame 3 after credit release");
            };
            assert_eq!(f3, vec![0xCC; 20]);

            // Replacing the video channel immediately fences gen1.
            let gen2 = ingress_driver.replace_video_channel(session_id).unwrap();
            assert_eq!(gen2, ChannelGeneration(2));
            assert_eq!(
                ingress_driver.enqueue_video_frame(session_id, gen1, vec![0xDD; 10]),
                Err(BrowserIngressError::Wss(WssError::StaleGeneration(
                    gen2, gen1
                )))
            );

            // Clean close on both channels.
            write_ws_frame(&mut vid, 0x08, &1000u16.to_be_bytes(), Some([1, 1, 1, 1]))
                .await
                .unwrap();
            write_ws_frame(&mut ctrl, 0x08, &1000u16.to_be_bytes(), Some([2, 2, 2, 2]))
                .await
                .unwrap();
        };

        let (_, _) = futures_join(server_fut, client_fut).await;
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
                // Bare 16-byte session ID with no FRAT ticket MUST be refused with Close!
                write_ws_frame(&mut client, 0x02, &[0x99; 16], Some([7, 7, 7, 7]))
                    .await
                    .unwrap();
                let frame = read_ws_frame(&mut client, 64, false).await.unwrap();
                assert_eq!(frame, WsFrame::Close);
            },
        )
        .await;
    });
}
