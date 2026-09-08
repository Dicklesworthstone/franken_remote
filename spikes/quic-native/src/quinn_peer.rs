//! The INDEPENDENT QUIC peer the bead requires: quinn 0.11 (quinn-proto state
//! machine, rustls 0.23, tokio runtime), sharing no QUIC implementation code
//! with asupersync. Test harness only — never a shipping dependency.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

pub const ECHO_STREAM_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct QuinnServerReport {
    pub handshake_ok: bool,
    pub negotiated_alpn: Option<Vec<u8>>,
    pub stream_bytes_echoed: u64,
    pub datagrams_echoed: u64,
    pub peer_error: Option<String>,
}

/// Run a one-connection echo server on its own thread + tokio runtime.
/// Returns (server address, report receiver, shutdown handle).
pub fn spawn_echo_server(
    leaf: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    alpn: &'static [u8],
) -> (
    SocketAddr,
    mpsc::Receiver<QuinnServerReport>,
    std::thread::JoinHandle<()>,
) {
    let (addr_tx, addr_rx) = mpsc::channel();
    let (report_tx, report_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        runtime.block_on(async move {
            let mut report = QuinnServerReport::default();
            let mut crypto = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![leaf], key)
                .expect("quinn server cert");
            crypto.alpn_protocols = vec![alpn.to_vec()];
            let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(crypto))
                .expect("quic server crypto");
            let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
            let transport =
                Arc::get_mut(&mut config.transport).expect("fresh transport config");
            transport.datagram_receive_buffer_size(Some(1 << 16));
            transport.max_idle_timeout(Some(
                quinn::IdleTimeout::try_from(Duration::from_secs(20)).expect("idle timeout"),
            ));
            let endpoint = quinn::Endpoint::server(config, "127.0.0.1:0".parse().unwrap())
                .expect("bind quinn server");
            addr_tx
                .send(endpoint.local_addr().expect("server addr"))
                .expect("report server addr");

            let outcome: Result<(), String> = async {
                let incoming = tokio::time::timeout(Duration::from_secs(30), endpoint.accept())
                    .await
                    .map_err(|_| "no incoming connection within 30s".to_string())?
                    .ok_or_else(|| "endpoint closed before a connection arrived".to_string())?;
                let connection = incoming
                    .await
                    .map_err(|e| format!("handshake failed: {e}"))?;
                report.handshake_ok = true;
                report.negotiated_alpn = connection
                    .handshake_data()
                    .and_then(|data| {
                        data.downcast::<quinn::crypto::rustls::HandshakeData>().ok()
                    })
                    .and_then(|data| data.protocol);

                // Echo datagrams until the connection goes away.
                let datagram_connection = connection.clone();
                let datagram_task = tokio::spawn(async move {
                    let mut echoed = 0u64;
                    while let Ok(datagram) = datagram_connection.read_datagram().await {
                        if datagram_connection.send_datagram(datagram).is_ok() {
                            echoed += 1;
                        }
                    }
                    echoed
                });

                // Echo exactly one bidirectional stream.
                let stream_bytes = match tokio::time::timeout(
                    Duration::from_secs(30),
                    connection.accept_bi(),
                )
                .await
                {
                    Ok(Ok((mut send, mut recv))) => {
                        let data = recv
                            .read_to_end(ECHO_STREAM_LIMIT)
                            .await
                            .map_err(|e| format!("stream read failed: {e}"))?;
                        send.write_all(&data)
                            .await
                            .map_err(|e| format!("stream echo failed: {e}"))?;
                        send.finish().map_err(|e| format!("finish failed: {e}"))?;
                        data.len() as u64
                    }
                    Ok(Err(e)) => return Err(format!("accept_bi failed: {e}")),
                    Err(_) => 0,
                };
                report.stream_bytes_echoed = stream_bytes;

                // Give the peer time to finish reading, then wind down.
                let _ = tokio::time::timeout(Duration::from_secs(15), connection.closed()).await;
                connection.close(0u32.into(), b"done");
                report.datagrams_echoed = datagram_task.await.unwrap_or(0);
                Ok(())
            }
            .await;
            if let Err(error) = outcome {
                report.peer_error = Some(error);
            }
            endpoint.wait_idle().await;
            let _ = report_tx.send(report);
        });
    });
    let addr = addr_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("quinn server never reported its address");
    (addr, report_rx, handle)
}

#[derive(Debug, Default)]
pub struct QuinnClientReport {
    pub handshake_ok: bool,
    pub negotiated_alpn: Option<Vec<u8>>,
    pub bytes_sent: u64,
    pub bytes_echoed: u64,
    pub echo_matches: bool,
    pub datagrams_sent: u64,
    pub datagrams_echoed: u64,
    pub max_datagram_size: Option<usize>,
    pub error: Option<String>,
}

/// Connect to `server_addr` as "localhost", push `stream_bytes` patterned bytes
/// through one bidi stream, expect the echo, and exchange a few datagrams.
pub fn spawn_echo_client(
    server_addr: SocketAddr,
    ca: CertificateDer<'static>,
    alpn: &'static [u8],
    stream_bytes: u64,
) -> (
    mpsc::Receiver<QuinnClientReport>,
    std::thread::JoinHandle<()>,
) {
    let (report_tx, report_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        runtime.block_on(async move {
            let mut report = QuinnClientReport::default();
            let outcome: Result<(), String> = async {
                let mut roots = rustls::RootCertStore::empty();
                roots.add(ca).map_err(|e| format!("root add: {e}"))?;
                let mut crypto = rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth();
                crypto.alpn_protocols = vec![alpn.to_vec()];
                let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(crypto))
                    .map_err(|e| format!("client crypto: {e}"))?;
                let mut config = quinn::ClientConfig::new(Arc::new(crypto));
                let mut transport = quinn::TransportConfig::default();
                transport.datagram_receive_buffer_size(Some(1 << 16));
                transport.max_idle_timeout(Some(
                    quinn::IdleTimeout::try_from(Duration::from_secs(20))
                        .map_err(|e| format!("idle timeout: {e}"))?,
                ));
                config.transport_config(Arc::new(transport));
                let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap())
                    .map_err(|e| format!("client endpoint: {e}"))?;
                endpoint.set_default_client_config(config);

                let connection = tokio::time::timeout(
                    Duration::from_secs(30),
                    endpoint
                        .connect(server_addr, "localhost")
                        .map_err(|e| format!("connect: {e}"))?,
                )
                .await
                .map_err(|_| "handshake timed out after 30s".to_string())?
                .map_err(|e| format!("handshake failed: {e}"))?;
                report.handshake_ok = true;
                report.negotiated_alpn = connection
                    .handshake_data()
                    .and_then(|data| {
                        data.downcast::<quinn::crypto::rustls::HandshakeData>().ok()
                    })
                    .and_then(|data| data.protocol);
                report.max_datagram_size = connection.max_datagram_size();

                let payload: Vec<u8> = (0..stream_bytes)
                    .map(|i| (i.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56) as u8)
                    .collect();
                let (mut send, mut recv) = connection
                    .open_bi()
                    .await
                    .map_err(|e| format!("open_bi: {e}"))?;
                send.write_all(&payload)
                    .await
                    .map_err(|e| format!("stream write: {e}"))?;
                send.finish().map_err(|e| format!("finish: {e}"))?;
                report.bytes_sent = payload.len() as u64;
                let echoed = tokio::time::timeout(
                    Duration::from_secs(60),
                    recv.read_to_end(ECHO_STREAM_LIMIT),
                )
                .await
                .map_err(|_| "echo read timed out after 60s".to_string())?
                .map_err(|e| format!("echo read: {e}"))?;
                report.bytes_echoed = echoed.len() as u64;
                report.echo_matches = echoed == payload;

                for size in [64usize, 512, 1000, 1150] {
                    let datagram: Vec<u8> = (0..size)
                        .map(|i| ((i as u64).wrapping_mul(31) >> 3) as u8)
                        .collect();
                    if connection
                        .send_datagram(bytes::Bytes::from(datagram))
                        .is_ok()
                    {
                        report.datagrams_sent += 1;
                    }
                }
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while report.datagrams_echoed < report.datagrams_sent {
                    match tokio::time::timeout_at(deadline, connection.read_datagram()).await {
                        Ok(Ok(_)) => report.datagrams_echoed += 1,
                        _ => break,
                    }
                }

                connection.close(0u32.into(), b"done");
                endpoint.wait_idle().await;
                Ok(())
            }
            .await;
            if let Err(error) = outcome {
                report.error = Some(error);
            }
            let _ = report_tx.send(report);
        });
    });
    (report_rx, handle)
}
