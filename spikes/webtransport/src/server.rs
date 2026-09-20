//! Live WebTransport / HTTP/3 server over Asupersync Native QUIC.

use std::net::SocketAddr;

use std::time::{Duration, Instant};

use asupersync::cx::Cx;
use asupersync::net::quic_core::{ConnectionId, TransportParameters, UnknownTransportParameter};
use asupersync::net::quic_native::handshake_driver::{QuicHandshakeDriver, server_config};
use asupersync::net::quic_native::{
    NativeQuicConnectionConfig, NativeQuicUdpConnection, QuicUdpEndpoint, QuicUdpEndpointConfig,
    StreamId,
};

use crate::h3;
use crate::pki::WebTransportPki;
use crate::proxy::Proxy;

pub fn server_transport_parameter_bytes(
    config: &NativeQuicConnectionConfig,
    local_cid: ConnectionId,
    original_destination_cid: Option<ConnectionId>,
) -> Vec<u8> {
    let mut connection_ids = vec![UnknownTransportParameter {
        id: 0x0f, // initial_source_connection_id
        value: local_cid.as_bytes().to_vec(),
    }];
    if let Some(original) = original_destination_cid {
        connection_ids.push(UnknownTransportParameter {
            id: 0x00, // original_destination_connection_id
            value: original.as_bytes().to_vec(),
        });
    }
    let parameters = TransportParameters {
        initial_max_data: Some(config.connection_recv_limit),
        initial_max_stream_data_bidi_local: Some(config.recv_window),
        initial_max_stream_data_bidi_remote: Some(config.recv_window),
        initial_max_stream_data_uni: Some(config.recv_window),
        initial_max_streams_bidi: Some(config.max_local_bidi),
        initial_max_streams_uni: Some(config.max_local_uni),
        max_datagram_frame_size: Some(65535),
        unknown: connection_ids,
        ..TransportParameters::default()
    };
    let mut bytes = Vec::new();
    parameters
        .encode(&mut bytes)
        .expect("encode transport parameters");
    bytes
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WebTransportSessionReport {
    pub client_alpn: String,
    pub connect_method: String,
    pub connect_protocol: String,
    pub connect_path: String,
    pub connect_origin: Option<String>,
    pub connect_draft: Option<String>,
    pub origin_accepted: bool,
    pub status_code: u16,
    pub datagrams_received: usize,
    pub datagrams_echoed: usize,
    pub max_datagram_payload_bytes: usize,
    pub uni_streams_received: usize,
    pub bidi_streams_echoed: usize,
    pub duration_ms: u64,
}

/// Serve a single WebTransport session.
pub async fn serve_one_session(
    cx: &Cx,
    pki: &WebTransportPki,
    expected_origin: Option<&str>,
    timeout: Duration,
    ready_tx: Option<std::sync::mpsc::Sender<SocketAddr>>,
) -> Result<(WebTransportSessionReport, SocketAddr), String> {
    let internal_endpoint = QuicUdpEndpoint::bind(
        cx,
        "127.0.0.1:0".parse().unwrap(),
        QuicUdpEndpointConfig::default(),
    )
    .await
    .map_err(|e| format!("endpoint bind: {e}"))?;

    let internal_addr = internal_endpoint.local_addr();

    // Spawn proxy on 127.0.0.1 to sniff Chrome's Initial DCID
    let proxy = Proxy::spawn(internal_addr).map_err(|e| format!("proxy spawn: {e}"))?;
    let client_facing_addr = proxy.client_facing;

    if let Some(tx) = ready_tx {
        let _ = tx.send(client_facing_addr);
    }

    println!(
        "Server listening via proxy on {} (internal {})",
        client_facing_addr, internal_addr
    );

    // Await client Initial DCID from proxy
    let initial_dcid_bytes = match proxy.initial_dcid.recv_timeout(timeout) {
        Ok(dcid) => dcid,
        Err(e) => return Err(format!("timed out waiting for client Initial packet: {e}")),
    };

    println!("Sniffed client Initial DCID: {:02x?}", initial_dcid_bytes);

    let initial_dcid = ConnectionId::new(&initial_dcid_bytes).map_err(|e| format!("dcid: {e}"))?;
    let local_cid = ConnectionId::new(b"fr-wt-serv-1").map_err(|e| format!("local cid: {e}"))?;

    let tls = server_config(
        vec![pki.cert_der.clone()],
        pki.key.clone_key(),
        vec![b"h3".to_vec()],
    )
    .map_err(|e| format!("server tls config: {e}"))?;

    let config = NativeQuicConnectionConfig {
        role: asupersync::net::quic_native::StreamRole::Server,
        max_local_bidi: 64,
        max_local_uni: 64,
        send_window: 1 << 20,
        recv_window: 1 << 20,
        connection_send_limit: 8 << 20,
        connection_recv_limit: 8 << 20,
        max_datagram_frame_size: 1400,
        drain_timeout_micros: 2_000_000,
    };

    let driver = QuicHandshakeDriver::server(
        tls,
        server_transport_parameter_bytes(&config, local_cid, Some(initial_dcid)),
    )
    .map_err(|e| format!("server driver: {e}"))?;

    println!("Accepting QUIC connection over UDP...");
    let mut conn = NativeQuicUdpConnection::accept(
        cx,
        internal_endpoint,
        driver,
        initial_dcid,
        local_cid,
        config,
        b"h3",
    )
    .await
    .map_err(|e| format!("accept failed: {e}"))?;

    let client_alpn = String::from_utf8_lossy(conn.negotiated_alpn()).to_string();
    println!("QUIC connection accepted! Negotiated ALPN: {}", client_alpn);

    // Open server control stream (unidirectional) and send SETTINGS
    let ctrl_id = conn
        .connection_mut()
        .open_uni_stream(cx)
        .map_err(|e| format!("open server control stream: {e}"))?;

    let ctrl_bytes = h3::server_control_stream_bytes();
    conn.connection_mut()
        .write_stream(
            cx,
            ctrl_id,
            asupersync::bytes::Bytes::copy_from_slice(&ctrl_bytes),
            false,
        )
        .map_err(|e| format!("write control settings: {e}"))?;

    conn.flush(cx)
        .await
        .map_err(|e| format!("flush server control stream: {e}"))?;

    let started = Instant::now();
    let session_deadline = started + timeout;

    let mut stream0_buf = Vec::new();
    let mut connect_req: Option<h3::WebTransportConnectRequest> = None;
    let mut origin_accepted = false;
    let mut status_code = 0u16;

    let mut datagrams_received = 0usize;
    let mut datagrams_echoed = 0usize;
    let mut max_datagram_payload_bytes = 0usize;
    let mut uni_streams_received = 0usize;
    let mut bidi_streams_echoed = 0usize;

    // Buffer for other streams: stream_id -> Vec<u8>
    let mut stream_buffers: std::collections::HashMap<u64, Vec<u8>> =
        std::collections::HashMap::new();

    while Instant::now() < session_deadline {
        let _ = conn.drive_io_once(cx, Duration::from_millis(5)).await;

        // Check incoming datagrams
        while let Some(raw_datagram) = conn.connection_mut().recv_datagram() {
            datagrams_received += 1;
            println!("Received QUIC datagram: {} bytes", raw_datagram.len());
            // WebTransport datagrams: decode quarter_stream_id (0)
            if let Ok(payload) = h3::decode_h3_datagram(0, &raw_datagram) {
                if payload.len() > max_datagram_payload_bytes {
                    max_datagram_payload_bytes = payload.len();
                }
                println!("Decoded H3 datagram payload: {} bytes", payload.len());
                // Echo datagram back
                let echo_raw = h3::encode_h3_datagram(0, &payload);
                if conn
                    .connection_mut()
                    .send_datagram(cx, echo_raw.into())
                    .is_ok()
                {
                    datagrams_echoed += 1;
                }
            }
        }

        // Read stream 0 for CONNECT request
        if connect_req.is_none() {
            match conn.connection_mut().read_stream(cx, StreamId(0), 16384) {
                Ok(bytes) => {
                    if !bytes.is_empty() {
                        println!(
                            "Read {} bytes from stream 0: {:02x?}",
                            bytes.len(),
                            &bytes[..]
                        );
                        stream0_buf.extend_from_slice(&bytes);
                        match h3::parse_connect_request(0, &stream0_buf) {
                            Ok(Some((req, _consumed))) => {
                                println!("Parsed CONNECT request: {:?}", req);
                                let origin_matches = match (expected_origin, &req.origin) {
                                    (Some(exp), Some(act)) => exp == "*" || exp == act,
                                    (Some(_), None) => false,
                                    (None, _) => true,
                                };

                                origin_accepted = origin_matches;
                                status_code = if origin_matches { 200 } else { 403 };

                                let resp = if origin_matches {
                                    h3::encode_connect_response_200(req.draft.as_deref())
                                } else {
                                    h3::encode_connect_response_403()
                                };

                                println!(
                                    "Sending CONNECT response (status {}): {:02x?}",
                                    status_code,
                                    &resp[..]
                                );
                                let _ = conn.connection_mut().write_stream(
                                    cx,
                                    StreamId(0),
                                    asupersync::bytes::Bytes::copy_from_slice(&resp),
                                    !origin_matches,
                                );
                                let _ = conn.flush(cx).await;

                                connect_req = Some(req);

                                if !origin_matches {
                                    println!(
                                        "Origin rejected: sending 403 and terminating session"
                                    );
                                    std::thread::sleep(Duration::from_millis(100));
                                    let _ = conn.flush(cx).await;
                                    break;
                                }
                            }
                            Ok(None) => {
                                println!(
                                    "parse_connect_request returned Ok(None) - waiting for more bytes"
                                );
                            }
                            Err(e) => {
                                println!("parse_connect_request ERROR: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    // Only log if not StreamUnknown / empty
                    static mut LOGGED_ERR: bool = false;
                    unsafe {
                        if !LOGGED_ERR {
                            println!("read_stream(0) err: {:?}", e);
                            LOGGED_ERR = true;
                        }
                    }
                }
            }
        }

        // Read other readable streams
        while let Ok(Some(readiness)) = conn.connection_mut().next_readable_stream(cx) {
            let sid = readiness.stream_id.0;
            if sid == 0 {
                continue; // Handled above
            }
            if let Ok(data) = conn
                .connection_mut()
                .read_stream(cx, readiness.stream_id, 16384)
                && !data.is_empty()
            {
                let buf = stream_buffers.entry(sid).or_default();
                buf.extend_from_slice(&data);
                println!(
                    "Stream {} read {} bytes: {:02x?}",
                    sid,
                    data.len(),
                    data.as_ref()
                );

                // If it's a client bidirectional stream (sid % 4 == 0, sid != 0)
                if sid % 4 == 0 {
                    // In draft-02, Chrome sends WEBTRANSPORT_STREAM frame (0x41) + session_id (0x00)
                    // as a preamble on client-initiated bidi streams.
                    // 0x41 is encoded as 2-byte varint [0x40, 0x41] followed by session_id [0x00].
                    let payload = if data.starts_with(&[0x40, 0x41, 0x00]) {
                        &data[3..]
                    } else if data.starts_with(&[0x41, 0x00]) {
                        &data[2..]
                    } else {
                        &data[..]
                    };
                    if !payload.is_empty() {
                        bidi_streams_echoed += 1;
                        println!(
                            "Echoing {} bytes on WT bidi stream {}: {:02x?}",
                            payload.len(),
                            sid,
                            payload
                        );
                        let _ = conn.connection_mut().write_stream(
                            cx,
                            readiness.stream_id,
                            asupersync::bytes::Bytes::copy_from_slice(payload),
                            false,
                        );
                        let _ = conn.flush(cx).await;
                    }
                } else if sid % 4 == 2 {
                    // Client unidirectional stream (sid % 4 == 2)
                    // Stream 2 is client control stream; other uni streams are WT streams
                    if sid > 2 {
                        uni_streams_received += 1;
                        println!("Received WT uni stream {} ({} bytes)", sid, data.len());
                    }
                }
            }
        }

        let _ = conn.flush(cx).await;

        // Check if connection is closing / closed
        if conn.connection().state() == asupersync::net::quic_native::QuicConnectionState::Closed
            || conn.connection().state()
                == asupersync::net::quic_native::QuicConnectionState::Draining
        {
            println!("Connection entered closed/draining state");
            break;
        }

        std::thread::sleep(Duration::from_millis(5));
    }

    let report = WebTransportSessionReport {
        client_alpn,
        connect_method: connect_req
            .as_ref()
            .map(|_| "CONNECT".to_string())
            .unwrap_or_default(),
        connect_protocol: connect_req
            .as_ref()
            .map(|_| "webtransport".to_string())
            .unwrap_or_default(),
        connect_path: connect_req
            .as_ref()
            .map(|r| r.path.clone())
            .unwrap_or_default(),
        connect_origin: connect_req.as_ref().and_then(|r| r.origin.clone()),
        connect_draft: connect_req.as_ref().and_then(|r| r.draft.clone()),
        origin_accepted,
        status_code,
        datagrams_received,
        datagrams_echoed,
        max_datagram_payload_bytes,
        uni_streams_received,
        bidi_streams_echoed,
        duration_ms: started.elapsed().as_millis() as u64,
    };

    Ok((report, client_facing_addr))
}
