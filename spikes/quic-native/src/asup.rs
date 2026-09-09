//! The composition under qualification: `QuicUdpEndpoint` (real UDP through the
//! Asupersync reactor) + `QuicHandshakeDriver` (rustls TLS 1.3) +
//! `NativeQuicUdpConnection` (authenticated 1-RTT data plane), exactly as
//! documented in asupersync's `net::quic_native::udp_connection`.
//!
//! Nothing here manually advances handshake state; every byte crosses a real
//! UDP socket.

use std::time::{Duration, Instant};

use asupersync::bytes::Bytes;
use asupersync::cx::Cx;
use asupersync::net::quic_core::{ConnectionId, TransportParameters, UnknownTransportParameter};
use asupersync::net::quic_native::handshake_driver::{
    QuicHandshakeDriver, client_config, server_config,
};
use asupersync::net::quic_native::{
    NativeQuicConnectionConfig, NativeQuicUdpConnection, QuicUdpEndpoint, QuicUdpEndpointConfig,
    StreamId,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use std::net::SocketAddr;

pub const ALPN: &[u8] = b"fr-spike/0";

/// Advertise receive limits and the actual connection IDs required by RFC 9000
/// section 7.3. The driver accepts caller-encoded parameters; its core subset
/// represents these standard byte-valued parameters through `unknown`.
pub fn transport_parameter_bytes(
    config: &NativeQuicConnectionConfig,
    local_cid: ConnectionId,
    original_destination_cid: Option<ConnectionId>,
) -> Vec<u8> {
    let mut connection_ids = vec![UnknownTransportParameter {
        id: 0x0f, // initial_source_connection_id (both endpoints)
        value: local_cid.as_bytes().to_vec(),
    }];
    if let Some(original) = original_destination_cid {
        connection_ids.push(UnknownTransportParameter {
            id: 0x00, // original_destination_connection_id (server only, no Retry)
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

#[test]
fn connection_id_parameters_use_actual_role_specific_bytes() {
    let config = NativeQuicConnectionConfig::default();
    let local = ConnectionId::new(b"local").unwrap();
    let original = ConnectionId::new(b"first").unwrap();
    let client = transport_parameter_bytes(&config, local, None);
    let server = transport_parameter_bytes(&config, local, Some(original));
    // RFC 9000 TLVs, independent of the upstream decoder: ID, byte length,
    // raw CID. No server-only original destination parameter on the client.
    assert!(client.ends_with(b"\x0f\x05local"));
    assert_eq!(server.len(), client.len() + 7);
    assert!(server.starts_with(&client));
    assert!(server.ends_with(b"\x00\x05first"));
}

pub async fn bind_endpoint(cx: &Cx, addr: &str) -> Result<QuicUdpEndpoint, String> {
    QuicUdpEndpoint::bind(
        cx,
        addr.parse().expect("literal socket address"),
        QuicUdpEndpointConfig::default(),
    )
    .await
    .map_err(|e| format!("endpoint bind failed: {e}"))
}

/// Complete a real client handshake against `server_addr`, verifying the server
/// certificate against `roots` for `server_name` and requiring [`ALPN`].
#[allow(clippy::too_many_arguments)]
pub async fn connect(
    cx: &Cx,
    endpoint: QuicUdpEndpoint,
    server_addr: SocketAddr,
    roots: Vec<CertificateDer<'static>>,
    server_name: &str,
    initial_dcid: &[u8],
    local_cid: &[u8],
    config: NativeQuicConnectionConfig,
) -> Result<NativeQuicUdpConnection, String> {
    let initial_dcid = ConnectionId::new(initial_dcid).map_err(|e| format!("initial dcid: {e}"))?;
    let local_cid = ConnectionId::new(local_cid).map_err(|e| format!("client cid: {e}"))?;
    let tls =
        client_config(roots, vec![ALPN.to_vec()]).map_err(|e| format!("client tls config: {e}"))?;
    let driver = QuicHandshakeDriver::client(
        tls,
        ServerName::try_from(server_name.to_string()).map_err(|e| format!("server name: {e}"))?,
        transport_parameter_bytes(&config, local_cid, None),
    )
    .map_err(|e| format!("client driver: {e}"))?;
    NativeQuicUdpConnection::connect(
        cx,
        endpoint,
        server_addr,
        driver,
        initial_dcid,
        local_cid,
        config,
        ALPN,
    )
    .await
    .map_err(|e| format!("client handshake failed: {e}"))
}

/// Complete a real server handshake on `endpoint` for a client whose Initial
/// carries `initial_dcid` (supplied out of band or by wire inspection — the
/// single-connection API does not discover it itself; that asymmetry is part of
/// the qualification evidence).
pub async fn accept(
    cx: &Cx,
    endpoint: QuicUdpEndpoint,
    leaf: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    initial_dcid: &[u8],
    local_cid: &[u8],
    config: NativeQuicConnectionConfig,
) -> Result<NativeQuicUdpConnection, String> {
    let initial_dcid = ConnectionId::new(initial_dcid).map_err(|e| format!("initial dcid: {e}"))?;
    let local_cid = ConnectionId::new(local_cid).map_err(|e| format!("server cid: {e}"))?;
    let tls = server_config(vec![leaf], key, vec![ALPN.to_vec()])
        .map_err(|e| format!("server tls config: {e}"))?;
    let driver = QuicHandshakeDriver::server(
        tls,
        transport_parameter_bytes(&config, local_cid, Some(initial_dcid)),
    )
    .map_err(|e| format!("server driver: {e}"))?;
    NativeQuicUdpConnection::accept(cx, endpoint, driver, initial_dcid, local_cid, config, ALPN)
        .await
        .map_err(|e| format!("server handshake failed: {e}"))
}

/// FNV-1a over a byte stream; enough to prove end-to-end integrity.
#[derive(Clone, Copy)]
pub struct Fnv(pub u64);

impl Fnv {
    pub fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

/// Deterministic payload pattern so both sides can checksum independently.
pub fn pattern_chunk(offset: u64, len: usize) -> Bytes {
    let mut out = Vec::with_capacity(len);
    for i in 0..len as u64 {
        let v = (offset + i).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        out.push((v >> 56) as u8);
    }
    Bytes::from(out)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TransferOutcome {
    pub bytes_sent: u64,
    pub bytes_echoed: u64,
    pub checksum_ok: bool,
    pub elapsed_ms: u64,
    pub client_lost: u64,
    pub client_acked: u64,
    pub client_pto: u32,
    pub smoothed_rtt_us: u64,
    pub cwnd: u64,
}

impl std::fmt::Display for TransferOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "bytes_sent={} bytes_echoed={} checksum_ok={} elapsed_ms={} client_lost={} client_acked={} client_pto={} smoothed_rtt_us={} cwnd={}",
            self.bytes_sent,
            self.bytes_echoed,
            self.checksum_ok,
            self.elapsed_ms,
            self.client_lost,
            self.client_acked,
            self.client_pto,
            self.smoothed_rtt_us,
            self.cwnd
        )
    }
}

/// Client writes `total` patterned bytes on one bidi stream; server echoes them
/// back on the same stream; client verifies the echo checksum. Both connections
/// are interleaved in one task; every byte crosses real UDP.
pub async fn echo_transfer(
    cx: &Cx,
    client: &mut NativeQuicUdpConnection,
    server: &mut NativeQuicUdpConnection,
    total: u64,
) -> Result<TransferOutcome, String> {
    const CHUNK: usize = 4096;
    const MAX_QUEUED: u64 = 64 * 1024;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(120);

    let stream = client
        .connection_mut()
        .open_control_stream(cx)
        .map_err(|e| format!("open stream: {e}"))?;

    let mut sent: u64 = 0;
    let mut sent_hash = Fnv::new();
    let mut echo_hash = Fnv::new();
    let mut expect_hash = Fnv::new();
    let mut echoed: u64 = 0;
    let mut server_stream: Option<StreamId> = None;
    let mut server_seen: u64 = 0;
    // Echo chunks the server has read but not yet been able to write back:
    // "flow control exhausted" from write_stream is backpressure (it queues
    // STREAM_DATA_BLOCKED), not failure, so the echo retries after I/O.
    let mut server_pending: std::collections::VecDeque<Bytes> = std::collections::VecDeque::new();

    while echoed < total {
        if Instant::now() > deadline {
            return Err(format!(
                "echo transfer timed out: sent={sent} server_seen={server_seen} echoed={echoed}"
            ));
        }

        // Client: keep the send queue primed without overrunning flow control.
        while sent < total && client.connection_mut().pending_stream_data_bytes(stream) < MAX_QUEUED
        {
            let len = CHUNK.min((total - sent) as usize);
            let chunk = pattern_chunk(sent, len);
            let fin = sent + len as u64 == total;
            match client
                .connection_mut()
                .write_stream(cx, stream, chunk.clone(), fin)
            {
                Ok(()) => {
                    sent_hash.update(&chunk);
                    sent += len as u64;
                }
                // Flow-control window exhausted: drive I/O and retry later.
                Err(_) => break,
            }
        }
        client
            .drive_io_once(cx, Duration::from_millis(5))
            .await
            .map_err(|e| format!("client drive: {e}"))?;

        // Server: discover the stream, then echo whatever is readable.
        if server_stream.is_none() {
            server_stream = server
                .connection_mut()
                .next_readable_stream(cx)
                .map_err(|e| format!("server next_readable_stream: {e}"))?
                .map(|readiness| readiness.stream_id);
        }
        if let Some(id) = server_stream {
            let mut read_any = false;
            loop {
                let bytes = server
                    .connection_mut()
                    .read_stream(cx, id, CHUNK)
                    .map_err(|e| format!("server read: {e}"))?;
                if bytes.is_empty() {
                    break;
                }
                read_any = true;
                server_seen += bytes.len() as u64;
                server_pending.push_back(bytes);
            }
            while let Some(front) = server_pending.front() {
                let fin = server_pending.len() == 1 && server_seen == total;
                match server
                    .connection_mut()
                    .write_stream(cx, id, front.clone(), fin)
                {
                    Ok(()) => {
                        server_pending.pop_front();
                    }
                    // Send window exhausted: retry after the next drive.
                    Err(_) => break,
                }
            }
            // Receive credit is caller-driven in this composition: slide the
            // stream and connection windows behind consumption or the peer
            // stalls at the initial 1 MiB stream window (measured).
            if read_any {
                server
                    .connection_mut()
                    .configure_stream_receive_window(cx, id, 1 << 20)
                    .map_err(|e| format!("server window: {e}"))?;
                server
                    .connection_mut()
                    .advertise_connection_receive_limit(cx, server_seen + (16 << 20))
                    .map_err(|e| format!("server MAX_DATA: {e}"))?;
            }
        }
        server
            .drive_io_once(cx, Duration::from_millis(5))
            .await
            .map_err(|e| format!("server drive: {e}"))?;

        // Client: consume the echo, sliding its own receive windows too.
        let mut read_any = false;
        loop {
            let bytes = client
                .connection_mut()
                .read_stream(cx, stream, CHUNK)
                .map_err(|e| format!("client echo read: {e}"))?;
            if bytes.is_empty() {
                break;
            }
            read_any = true;
            echo_hash.update(&bytes);
            echoed += bytes.len() as u64;
        }
        if read_any {
            client
                .connection_mut()
                .configure_stream_receive_window(cx, stream, 1 << 20)
                .map_err(|e| format!("client window: {e}"))?;
            client
                .connection_mut()
                .advertise_connection_receive_limit(cx, echoed + (16 << 20))
                .map_err(|e| format!("client MAX_DATA: {e}"))?;
        }
    }
    // The echo must byte-match the pattern the client sent.
    let mut offset = 0u64;
    while offset < total {
        let len = CHUNK.min((total - offset) as usize);
        expect_hash.update(&pattern_chunk(offset, len));
        offset += len as u64;
    }

    let stats = client.connection_mut().path_stats();
    Ok(TransferOutcome {
        bytes_sent: sent,
        bytes_echoed: echoed,
        checksum_ok: sent_hash.0 == expect_hash.0 && echo_hash.0 == expect_hash.0,
        elapsed_ms: started.elapsed().as_millis() as u64,
        client_lost: stats.packets_lost,
        client_acked: stats.packets_acked,
        client_pto: stats.pto_count,
        smoothed_rtt_us: stats.smoothed_rtt_micros.unwrap_or(0),
        cwnd: stats.congestion_window_bytes,
    })
}

#[derive(Debug, Default)]
pub struct DatagramProbeOutcome {
    /// Largest payload `send_datagram` accepted.
    pub max_accepted: usize,
    /// Smallest payload `send_datagram` refused up front (0 = none refused).
    pub min_refused: usize,
    /// Largest payload actually delivered intact to the peer.
    pub max_delivered: usize,
    /// Smallest payload the sender ADMITTED but that then killed the drive
    /// path at packet assembly (0 = never happened). A nonzero value is an
    /// upstream admission/assembly mismatch finding.
    pub lethal_admitted_size: usize,
    pub lethal_error: Option<String>,
    pub sent_count: u64,
    pub delivered_count: u64,
    pub integrity_ok: bool,
}

/// Probe RFC 9221 datagram limits size by size: what the sender admits, what
/// actually arrives intact, and whether any admitted size is lethal downstream.
pub async fn datagram_probe(
    cx: &Cx,
    sender: &mut NativeQuicUdpConnection,
    receiver: &mut NativeQuicUdpConnection,
) -> Result<DatagramProbeOutcome, String> {
    let sizes: &[usize] = &[
        64, 256, 512, 1024, 1100, 1150, 1180, 1200, 1232, 1250, 1350, 2048, 4096, 65527,
    ];
    let mut out = DatagramProbeOutcome {
        integrity_ok: true,
        ..DatagramProbeOutcome::default()
    };

    'sizes: for &size in sizes {
        let payload = pattern_chunk(size as u64, size);
        match sender.connection_mut().send_datagram(cx, payload.clone()) {
            Ok(()) => {
                out.max_accepted = out.max_accepted.max(size);
                out.sent_count += 1;
            }
            Err(_) => {
                if out.min_refused == 0 {
                    out.min_refused = size;
                }
                continue;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut delivered = false;
        while !delivered && Instant::now() < deadline {
            if let Err(error) = sender.drive_io_once(cx, Duration::from_millis(5)).await {
                out.lethal_admitted_size = size;
                out.lethal_error = Some(format!("sender drive after admitted send: {error}"));
                break 'sizes;
            }
            if let Err(error) = receiver.drive_io_once(cx, Duration::from_millis(5)).await {
                out.lethal_admitted_size = size;
                out.lethal_error = Some(format!("receiver drive: {error}"));
                break 'sizes;
            }
            while let Some(datagram) = receiver.connection_mut().recv_datagram() {
                delivered = true;
                out.delivered_count += 1;
                if datagram.len() == size && datagram[..] == payload[..] {
                    out.max_delivered = out.max_delivered.max(datagram.len());
                } else {
                    out.integrity_ok = false;
                }
            }
        }
    }
    Ok(out)
}

/// utime+stime of this process in microseconds; missing counters are an error.
pub fn process_cpu_micros(ticks_per_second: u64) -> Result<u64, String> {
    let stat =
        std::fs::read_to_string("/proc/self/stat").map_err(|e| format!("CPU counter: {e}"))?;
    // Fields 14 and 15 (1-based) are utime/stime in clock ticks; the comm field
    // is parenthesized and may contain spaces, so split after the last ')'.
    let after = stat
        .rsplit_once(')')
        .map(|(_, rest)| rest)
        .ok_or("invalid CPU counter")?;
    let fields: Vec<&str> = after.split_whitespace().collect();
    let utime: u64 = fields
        .get(11)
        .and_then(|v| v.parse().ok())
        .ok_or("missing utime")?;
    let stime: u64 = fields
        .get(12)
        .and_then(|v| v.parse().ok())
        .ok_or("missing stime")?;
    utime
        .checked_add(stime)
        .and_then(|ticks| ticks.checked_mul(1_000_000))
        .and_then(|micros| micros.checked_div(ticks_per_second))
        .ok_or_else(|| "invalid CPU counter scale".to_string())
}

#[derive(Debug, Default, Clone, Copy)]
pub struct IdleOutcome {
    pub wall_ms: u64,
    pub cpu_ms: u64,
    pub cpu_fraction_percent: u64,
    pub wakeups: u64,
}

impl std::fmt::Display for IdleOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "wall_ms={} cpu_ms={} cpu_fraction_percent={} wakeups={}",
            self.wall_ms, self.cpu_ms, self.cpu_fraction_percent, self.wakeups
        )
    }
}

/// Hold an established connection open with a silent peer and measure how much
/// CPU the receive path burns: reactor suspension vs busy-poll, measured — not
/// inferred from async-looking signatures.
pub async fn idle_watch(
    cx: &Cx,
    conn: &mut NativeQuicUdpConnection,
    wall: Duration,
) -> Result<IdleOutcome, String> {
    let clock = std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|e| format!("CPU clock: {e}"))?;
    if !clock.status.success() {
        return Err("getconf CLK_TCK failed".to_string());
    }
    let ticks_per_second = std::str::from_utf8(&clock.stdout)
        .map_err(|_| "invalid CLK_TCK")?
        .trim()
        .parse()
        .map_err(|_| "invalid CLK_TCK")?;
    let started = Instant::now();
    let cpu_before = process_cpu_micros(ticks_per_second)?;
    let mut wakeups = 0u64;
    while started.elapsed() < wall {
        conn.drive_io_once(cx, Duration::from_millis(500))
            .await
            .map_err(|e| format!("idle drive: {e}"))?;
        wakeups += 1;
    }
    let wall_us = started.elapsed().as_micros() as u64;
    let cpu_us = process_cpu_micros(ticks_per_second)?
        .checked_sub(cpu_before)
        .ok_or("CPU counter moved backwards")?;
    Ok(IdleOutcome {
        wall_ms: wall_us / 1000,
        cpu_ms: cpu_us / 1000,
        cpu_fraction_percent: cpu_us * 100 / wall_us.max(1),
        wakeups,
    })
}
