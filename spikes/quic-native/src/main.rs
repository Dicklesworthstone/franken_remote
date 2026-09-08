//! Phase 0 gate `fr-p0-quic-native-aqy`: qualify one live Asupersync QUIC
//! endpoint composition end to end (plan §4.1, §12.1, §12.4, §23 Phase 0).
//!
//! One scenario per process invocation so measurements (CPU, sockets, threads)
//! stay attributable. Every scenario prints greppable evidence lines:
//!
//! `RESULT scenario=<name> row=<row> status=<passed|failed|blocked> detail="…"`
//!
//! A failed row is a finding, not an error in this harness; the process exits
//! zero unless the harness itself is broken.

mod asup;
mod certs;
mod proxy;
mod quinn_peer;

use std::time::{Duration, Instant};

use asup::ALPN;
use asupersync::cx::Cx;
use asupersync::net::quic_native::{NativeQuicConnectionConfig, NativeQuicUdpConnection};
use certs::TestPki;
use futures_lite::future::{block_on, zip};

fn result_row(scenario: &str, row: &str, status: &str, detail: &str) {
    println!("RESULT scenario={scenario} row={row} status={status} detail=\"{detail}\"");
}

fn main() {
    let scenario = std::env::args().nth(1).unwrap_or_default();
    match scenario.as_str() {
        "self-pair" => self_pair(),
        "tls-negative" => tls_negative(),
        "idle-cpu" => idle_cpu(),
        "loss" => loss(),
        "cancel" => cancel(),
        "interop-quinn-server" => interop_quinn_server(),
        "interop-quinn-client" => interop_quinn_client(),
        other => {
            eprintln!(
                "unknown scenario {other:?}; expected one of: self-pair | tls-negative | \
                 idle-cpu | loss | cancel | interop-quinn-server | interop-quinn-client"
            );
            std::process::exit(2);
        }
    }
}

/// Establish an asupersync client/server pair over real loopback UDP,
/// optionally with the client aimed at a middlebox address instead of the
/// server's own.
fn establish_pair(
    cx: &Cx,
    pki: &TestPki,
    client_target_override: Option<std::net::SocketAddr>,
) -> Result<(NativeQuicUdpConnection, NativeQuicUdpConnection), String> {
    block_on(async {
        let config = NativeQuicConnectionConfig::default();
        let client_endpoint = asup::bind_endpoint(cx, "127.0.0.1:0").await?;
        let server_endpoint = asup::bind_endpoint(cx, "127.0.0.1:0").await?;
        let server_addr = server_endpoint.local_addr();
        let target = client_target_override.unwrap_or(server_addr);
        let (client, server) = zip(
            asup::connect(
                cx,
                client_endpoint,
                target,
                vec![pki.ca_der.clone()],
                "localhost",
                b"spike-i1",
                b"spike-c1",
                config,
            ),
            asup::accept(
                cx,
                server_endpoint,
                pki.leaf_der.clone(),
                pki.leaf_key.clone_key(),
                b"spike-i1",
                b"spike-s1",
                config,
            ),
        )
        .await;
        Ok((client?, server?))
    })
}

fn self_pair() {
    let scenario = "self-pair";
    let pki = TestPki::generate();
    let cx = Cx::for_testing();
    let started = Instant::now();
    let (mut client, mut server) = match establish_pair(&cx, &pki, None) {
        Ok(pair) => pair,
        Err(error) => {
            result_row(scenario, "handshake", "failed", &error);
            return;
        }
    };
    result_row(
        scenario,
        "handshake",
        "passed",
        &format!(
            "established in {} ms; alpn client={:?} server={:?}; cids client={:?}/{:?}",
            started.elapsed().as_millis(),
            String::from_utf8_lossy(client.negotiated_alpn()),
            String::from_utf8_lossy(server.negotiated_alpn()),
            client.local_connection_id(),
            client.peer_connection_id(),
        ),
    );

    match block_on(asup::echo_transfer(&cx, &mut client, &mut server, 4 << 20)) {
        Ok(outcome) => {
            let status = if outcome.checksum_ok && outcome.bytes_echoed == 4 << 20 {
                "passed"
            } else {
                "failed"
            };
            result_row(
                scenario,
                "stream-echo-4mib",
                status,
                &format!("{outcome:?}"),
            );
        }
        Err(error) => result_row(scenario, "stream-echo-4mib", "failed", &error),
    }

    match block_on(asup::datagram_probe(&cx, &mut client, &mut server)) {
        Ok(outcome) => {
            let status = if outcome.integrity_ok
                && outcome.delivered_count > 0
                && outcome.lethal_admitted_size == 0
            {
                "passed"
            } else {
                "failed"
            };
            result_row(scenario, "datagram-probe", status, &format!("{outcome:?}"));
        }
        Err(error) => result_row(scenario, "datagram-probe", "failed", &error),
    }
}

fn tls_negative() {
    let scenario = "tls-negative";
    let pki = TestPki::generate();
    let cx = Cx::for_testing();

    // Wrong trust anchor: client trusts a CA that did not sign the leaf.
    let outcome = block_on(async {
        let config = NativeQuicConnectionConfig::default();
        let client_endpoint = asup::bind_endpoint(&cx, "127.0.0.1:0").await?;
        let server_endpoint = asup::bind_endpoint(&cx, "127.0.0.1:0").await?;
        let server_addr = server_endpoint.local_addr();
        let (client, _server) = zip(
            asup::connect(
                &cx,
                client_endpoint,
                server_addr,
                vec![pki.wrong_ca_der.clone()],
                "localhost",
                b"spike-i2",
                b"spike-c2",
                config,
            ),
            asup::accept(
                &cx,
                server_endpoint,
                pki.leaf_der.clone(),
                pki.leaf_key.clone_key(),
                b"spike-i2",
                b"spike-s2",
                config,
            ),
        )
        .await;
        Ok::<_, String>(client)
    });
    match outcome {
        Ok(Err(reason)) => result_row(
            scenario,
            "untrusted-ca-refused",
            "passed",
            &format!("client refused as required: {reason}"),
        ),
        Ok(Ok(_)) => result_row(
            scenario,
            "untrusted-ca-refused",
            "failed",
            "handshake SUCCEEDED against an untrusted certificate chain",
        ),
        Err(error) => result_row(scenario, "untrusted-ca-refused", "failed", &error),
    }

    // Wrong hostname: trust anchor is right, requested server name is not.
    let outcome = block_on(async {
        let config = NativeQuicConnectionConfig::default();
        let client_endpoint = asup::bind_endpoint(&cx, "127.0.0.1:0").await?;
        let server_endpoint = asup::bind_endpoint(&cx, "127.0.0.1:0").await?;
        let server_addr = server_endpoint.local_addr();
        let (client, _server) = zip(
            asup::connect(
                &cx,
                client_endpoint,
                server_addr,
                vec![pki.ca_der.clone()],
                "not-the-server.invalid",
                b"spike-i3",
                b"spike-c3",
                config,
            ),
            asup::accept(
                &cx,
                server_endpoint,
                pki.leaf_der.clone(),
                pki.leaf_key.clone_key(),
                b"spike-i3",
                b"spike-s3",
                config,
            ),
        )
        .await;
        Ok::<_, String>(client)
    });
    match outcome {
        Ok(Err(reason)) => result_row(
            scenario,
            "wrong-hostname-refused",
            "passed",
            &format!("client refused as required: {reason}"),
        ),
        Ok(Ok(_)) => result_row(
            scenario,
            "wrong-hostname-refused",
            "failed",
            "handshake SUCCEEDED for a hostname the certificate does not name",
        ),
        Err(error) => result_row(scenario, "wrong-hostname-refused", "failed", &error),
    }
}

fn idle_cpu() {
    let scenario = "idle-cpu";
    let pki = TestPki::generate();
    let cx = Cx::for_testing();
    let (mut client, mut server) = match establish_pair(&cx, &pki, None) {
        Ok(pair) => pair,
        Err(error) => {
            result_row(scenario, "handshake", "failed", &error);
            return;
        }
    };
    // Quiesce: let post-handshake acknowledgements settle.
    let _ = block_on(async {
        for _ in 0..10 {
            client
                .drive_io_once(&cx, Duration::from_millis(10))
                .await
                .map_err(|e| e.to_string())?;
            server
                .drive_io_once(&cx, Duration::from_millis(10))
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok::<_, String>(())
    });
    match block_on(asup::idle_watch(&cx, &mut client, Duration::from_secs(6))) {
        Ok(outcome) => {
            // Busy-polling burns ~100% of one core; a suspending reactor burns
            // almost nothing. 20% is a generous dividing line.
            let status = if outcome.cpu_fraction_percent < 20 {
                "passed"
            } else {
                "failed"
            };
            result_row(
                scenario,
                "receive-suspends-not-busy-polls",
                status,
                &format!("{outcome:?}"),
            );
        }
        Err(error) => result_row(scenario, "receive-suspends-not-busy-polls", "failed", &error),
    }
}

fn loss() {
    let scenario = "loss";
    let pki = TestPki::generate();
    let cx = Cx::for_testing();
    let server_probe = block_on(asup::bind_endpoint(&cx, "127.0.0.1:0"));
    let server_endpoint = match server_probe {
        Ok(endpoint) => endpoint,
        Err(error) => {
            result_row(scenario, "setup", "failed", &error);
            return;
        }
    };
    let server_addr = server_endpoint.local_addr();
    // 3% loss + 2% adjacent reorder in both directions, deterministic seed.
    let middlebox = match proxy::Proxy::spawn(server_addr, 30, 20, 0x5eed_1234) {
        Ok(proxy) => proxy,
        Err(error) => {
            result_row(scenario, "setup", "failed", &format!("proxy: {error}"));
            return;
        }
    };

    let outcome = block_on(async {
        let config = NativeQuicConnectionConfig::default();
        let client_endpoint = asup::bind_endpoint(&cx, "127.0.0.1:0").await?;
        let (client, server) = zip(
            asup::connect(
                &cx,
                client_endpoint,
                middlebox.client_facing,
                vec![pki.ca_der.clone()],
                "localhost",
                b"spike-i4",
                b"spike-c4",
                config,
            ),
            asup::accept(
                &cx,
                server_endpoint,
                pki.leaf_der.clone(),
                pki.leaf_key.clone_key(),
                b"spike-i4",
                b"spike-s4",
                config,
            ),
        )
        .await;
        Ok::<_, String>((client?, server?))
    });
    let (mut client, mut server) = match outcome {
        Ok(pair) => {
            result_row(
                scenario,
                "handshake-under-loss",
                "passed",
                "established through 3% loss / 2% reorder middlebox",
            );
            pair
        }
        Err(error) => {
            result_row(scenario, "handshake-under-loss", "failed", &error);
            middlebox.shutdown();
            return;
        }
    };

    match block_on(asup::echo_transfer(&cx, &mut client, &mut server, 1 << 20)) {
        Ok(outcome) => {
            let status = if outcome.checksum_ok && outcome.bytes_echoed == 1 << 20 {
                "passed"
            } else {
                "failed"
            };
            result_row(
                scenario,
                "stream-recovers-under-loss",
                status,
                &format!(
                    "{outcome:?} proxy_dropped_c2s={} proxy_dropped_s2c={} proxy_reordered={}",
                    middlebox
                        .stats
                        .dropped_c2s
                        .load(std::sync::atomic::Ordering::Relaxed),
                    middlebox
                        .stats
                        .dropped_s2c
                        .load(std::sync::atomic::Ordering::Relaxed),
                    middlebox
                        .stats
                        .reordered
                        .load(std::sync::atomic::Ordering::Relaxed),
                ),
            );
        }
        Err(error) => result_row(scenario, "stream-recovers-under-loss", "failed", &error),
    }
    middlebox.shutdown();
}

fn cancel() {
    let scenario = "cancel";
    let pki = TestPki::generate();
    let cx = Cx::for_testing();
    let (mut client, _server) = match establish_pair(&cx, &pki, None) {
        Ok(pair) => pair,
        Err(error) => {
            result_row(scenario, "handshake", "failed", &error);
            return;
        }
    };
    cx.set_cancel_requested(true);
    let started = Instant::now();
    let drive = block_on(client.drive_io_once(&cx, Duration::from_secs(30)));
    let elapsed = started.elapsed();
    let refused_fast = drive.is_err() && elapsed < Duration::from_secs(1);
    let write = client
        .connection_mut()
        .send_datagram(&cx, asup::pattern_chunk(0, 64));
    match (refused_fast, write.is_err()) {
        (true, true) => result_row(
            scenario,
            "cancel-refuses-promptly",
            "passed",
            &format!(
                "drive_io refused in {:?} ({}); post-cancel send refused ({})",
                elapsed,
                drive.err().map(|e| e.to_string()).unwrap_or_default(),
                write.err().map(|e| e.to_string()).unwrap_or_default(),
            ),
        ),
        _ => result_row(
            scenario,
            "cancel-refuses-promptly",
            "failed",
            &format!(
                "drive_err={:?} in {:?}; send_err={:?}",
                drive.err().map(|e| e.to_string()),
                elapsed,
                write.err().map(|e| e.to_string()),
            ),
        ),
    }
}

/// Single-sided client transfer against an independently driven remote peer.
fn client_echo_against_remote(
    cx: &Cx,
    client: &mut NativeQuicUdpConnection,
    total: u64,
) -> Result<asup::TransferOutcome, String> {
    block_on(async {
        const CHUNK: usize = 4096;
        const MAX_QUEUED: u64 = 64 * 1024;
        let started = Instant::now();
        let deadline = started + Duration::from_secs(120);
        let stream = client
            .connection_mut()
            .open_control_stream(cx)
            .map_err(|e| format!("open stream: {e}"))?;
        let mut sent = 0u64;
        let mut echoed = 0u64;
        let mut sent_hash = asup::Fnv::new();
        let mut echo_hash = asup::Fnv::new();
        while echoed < total {
            if Instant::now() > deadline {
                return Err(format!("timed out: sent={sent} echoed={echoed}"));
            }
            while sent < total
                && client.connection_mut().pending_stream_data_bytes(stream) < MAX_QUEUED
            {
                let len = CHUNK.min((total - sent) as usize);
                let chunk = asup::pattern_chunk(sent, len);
                let fin = sent + len as u64 == total;
                match client
                    .connection_mut()
                    .write_stream(cx, stream, chunk.clone(), fin)
                {
                    Ok(()) => {
                        sent_hash.update(&chunk);
                        sent += len as u64;
                    }
                    Err(_) => break,
                }
            }
            client
                .drive_io_once(cx, Duration::from_millis(5))
                .await
                .map_err(|e| format!("drive: {e}"))?;
            let mut read_any = false;
            loop {
                let bytes = client
                    .connection_mut()
                    .read_stream(cx, stream, CHUNK)
                    .map_err(|e| format!("echo read: {e}"))?;
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
                    .map_err(|e| format!("window: {e}"))?;
                client
                    .connection_mut()
                    .advertise_connection_receive_limit(cx, echoed + (16 << 20))
                    .map_err(|e| format!("MAX_DATA: {e}"))?;
            }
        }
        let stats = client.connection_mut().path_stats();
        Ok(asup::TransferOutcome {
            bytes_sent: sent,
            bytes_echoed: echoed,
            checksum_ok: sent_hash.0 == echo_hash.0,
            elapsed_ms: started.elapsed().as_millis() as u64,
            client_lost: stats.packets_lost,
            client_acked: stats.packets_acked,
            client_pto: stats.pto_count,
            smoothed_rtt_us: stats.smoothed_rtt_micros.unwrap_or(0),
            cwnd: stats.congestion_window_bytes,
        })
    })
}

fn interop_quinn_server() {
    let scenario = "interop-quinn-server";
    let pki = TestPki::generate();
    let cx = Cx::for_testing();
    let (server_addr, report_rx, server_thread) =
        quinn_peer::spawn_echo_server(pki.leaf_der.clone(), pki.leaf_key.clone_key(), ALPN);

    let connected = block_on(async {
        let config = NativeQuicConnectionConfig::default();
        let endpoint = asup::bind_endpoint(&cx, "127.0.0.1:0").await?;
        asup::connect(
            &cx,
            endpoint,
            server_addr,
            vec![pki.ca_der.clone()],
            "localhost",
            b"spike-i5",
            b"spike-c5",
            config,
        )
        .await
    });
    let mut client = match connected {
        Ok(client) => {
            result_row(
                scenario,
                "handshake-against-independent-server",
                "passed",
                &format!(
                    "asupersync client completed a real handshake with quinn 0.11; alpn={:?}",
                    String::from_utf8_lossy(client.negotiated_alpn())
                ),
            );
            client
        }
        Err(error) => {
            result_row(scenario, "handshake-against-independent-server", "failed", &error);
            drop(report_rx);
            let _ = server_thread.join();
            return;
        }
    };

    match client_echo_against_remote(&cx, &mut client, 1 << 20) {
        Ok(outcome) => {
            let status = if outcome.checksum_ok && outcome.bytes_echoed == 1 << 20 {
                "passed"
            } else {
                "failed"
            };
            result_row(scenario, "stream-echo-1mib", status, &format!("{outcome:?}"));
        }
        Err(error) => result_row(scenario, "stream-echo-1mib", "failed", &error),
    }

    // Datagrams: send a few, count echoes from the independent peer.
    let datagram_outcome = block_on(async {
        let mut sent = 0u64;
        let mut echoed = 0u64;
        for size in [64usize, 512, 1000, 1150] {
            if client
                .connection_mut()
                .send_datagram(&cx, asup::pattern_chunk(size as u64, size))
                .is_ok()
            {
                sent += 1;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while echoed < sent && Instant::now() < deadline {
            client
                .drive_io_once(&cx, Duration::from_millis(10))
                .await
                .map_err(|e| format!("drive: {e}"))?;
            while client.connection_mut().recv_datagram().is_some() {
                echoed += 1;
            }
        }
        Ok::<_, String>((sent, echoed))
    });
    match datagram_outcome {
        Ok((sent, echoed)) if sent > 0 && echoed == sent => result_row(
            scenario,
            "datagram-roundtrip",
            "passed",
            &format!("{echoed}/{sent} datagrams echoed by quinn"),
        ),
        Ok((sent, echoed)) => result_row(
            scenario,
            "datagram-roundtrip",
            "failed",
            &format!("{echoed}/{sent} datagrams echoed"),
        ),
        Err(error) => result_row(scenario, "datagram-roundtrip", "failed", &error),
    }

    // Close so the server thread can wind down, then collect its report.
    let _ = client.connection_mut().begin_close(&cx, 0, 0);
    let _ = block_on(client.flush(&cx));
    match report_rx.recv_timeout(Duration::from_secs(40)) {
        Ok(report) => result_row(
            scenario,
            "independent-peer-view",
            if report.handshake_ok { "passed" } else { "failed" },
            &format!("{report:?}"),
        ),
        Err(_) => result_row(
            scenario,
            "independent-peer-view",
            "failed",
            "quinn server never reported",
        ),
    }
    let _ = server_thread.join();
}

fn interop_quinn_client() {
    let scenario = "interop-quinn-client";
    let pki = TestPki::generate();
    let cx = Cx::for_testing();

    let server_endpoint = match block_on(asup::bind_endpoint(&cx, "127.0.0.1:0")) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            result_row(scenario, "setup", "failed", &error);
            return;
        }
    };
    let server_addr = server_endpoint.local_addr();
    // Lossless middlebox whose only job is sniffing the client's Initial DCID,
    // because the single-connection accept API needs it up front.
    let middlebox = match proxy::Proxy::spawn(server_addr, 0, 0, 1) {
        Ok(proxy) => proxy,
        Err(error) => {
            result_row(scenario, "setup", "failed", &format!("proxy: {error}"));
            return;
        }
    };
    let (client_report_rx, client_thread) = quinn_peer::spawn_echo_client(
        middlebox.client_facing,
        pki.ca_der.clone(),
        ALPN,
        1 << 20,
    );

    let initial_dcid = match middlebox.initial_dcid.recv_timeout(Duration::from_secs(10)) {
        Ok(dcid) => dcid,
        Err(_) => {
            result_row(
                scenario,
                "initial-dcid-sniff",
                "failed",
                "no client Initial observed within 10s",
            );
            middlebox.shutdown();
            let _ = client_thread.join();
            return;
        }
    };

    let accepted = block_on(asup::accept(
        &cx,
        server_endpoint,
        pki.leaf_der.clone(),
        pki.leaf_key.clone_key(),
        &initial_dcid,
        b"spike-s6",
        NativeQuicConnectionConfig::default(),
    ));
    let mut server = match accepted {
        Ok(server) => {
            result_row(
                scenario,
                "handshake-from-independent-client",
                "passed",
                &format!(
                    "asupersync server admitted quinn 0.11 client; alpn={:?} dcid_len={}",
                    String::from_utf8_lossy(server.negotiated_alpn()),
                    initial_dcid.len()
                ),
            );
            server
        }
        Err(error) => {
            result_row(scenario, "handshake-from-independent-client", "failed", &error);
            middlebox.shutdown();
            let _ = client_thread.join();
            return;
        }
    };

    // Serve: echo the stream (mirroring FIN) and echo datagrams until the
    // independent client reports.
    let serve_outcome = block_on(async {
        use asupersync::net::quic_native::StreamId;
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut stream: Option<StreamId> = None;
        let mut echoed = 0u64;
        let mut fin_echoed = false;
        let mut eof_seen = false;
        let mut pending: std::collections::VecDeque<asupersync::bytes::Bytes> =
            std::collections::VecDeque::new();
        loop {
            if Instant::now() > deadline {
                return Err(format!("serve timed out: echoed={echoed}"));
            }
            server
                .drive_io_once(&cx, Duration::from_millis(5))
                .await
                .map_err(|e| format!("drive: {e}"))?;
            if stream.is_none() {
                stream = server
                    .connection_mut()
                    .next_readable_stream(&cx)
                    .map_err(|e| format!("next_readable_stream: {e}"))?
                    .map(|readiness| readiness.stream_id);
            }
            if let Some(id) = stream {
                let mut read_any = false;
                loop {
                    let bytes = server
                        .connection_mut()
                        .read_stream(&cx, id, 4096)
                        .map_err(|e| format!("read: {e}"))?;
                    if bytes.is_empty() {
                        break;
                    }
                    read_any = true;
                    echoed += bytes.len() as u64;
                    pending.push_back(bytes);
                }
                if read_any {
                    server
                        .connection_mut()
                        .configure_stream_receive_window(&cx, id, 1 << 20)
                        .map_err(|e| format!("window: {e}"))?;
                    server
                        .connection_mut()
                        .advertise_connection_receive_limit(&cx, echoed + (16 << 20))
                        .map_err(|e| format!("MAX_DATA: {e}"))?;
                }
                if !eof_seen {
                    eof_seen = server.connection_mut().is_stream_eof(id).unwrap_or(false);
                }
                // Flush the echo, treating flow-control refusals as
                // backpressure; FIN rides the final chunk once EOF is seen.
                while let Some(front) = pending.front() {
                    let fin = eof_seen && pending.len() == 1;
                    match server
                        .connection_mut()
                        .write_stream(&cx, id, front.clone(), fin)
                    {
                        Ok(()) => {
                            fin_echoed = fin;
                            pending.pop_front();
                        }
                        Err(_) => break,
                    }
                }
                if eof_seen && pending.is_empty() && !fin_echoed {
                    if server
                        .connection_mut()
                        .write_stream(&cx, id, asupersync::bytes::Bytes::new(), true)
                        .is_ok()
                    {
                        fin_echoed = true;
                    }
                }
            }
            while let Some(datagram) = server.connection_mut().recv_datagram() {
                let _ = server.connection_mut().send_datagram(&cx, datagram);
            }
            if let Ok(report) = client_report_rx.try_recv() {
                return Ok((echoed, report));
            }
        }
    });

    match serve_outcome {
        Ok((echoed, report)) => {
            let stream_ok = report.echo_matches && report.bytes_echoed == 1 << 20;
            result_row(
                scenario,
                "stream-echo-1mib",
                if stream_ok { "passed" } else { "failed" },
                &format!("server_echoed={echoed} client_view={report:?}"),
            );
            result_row(
                scenario,
                "datagram-roundtrip",
                if report.datagrams_sent > 0 && report.datagrams_echoed == report.datagrams_sent {
                    "passed"
                } else {
                    "failed"
                },
                &format!(
                    "{}/{} datagrams echoed back to quinn (its max_datagram_size={:?})",
                    report.datagrams_echoed, report.datagrams_sent, report.max_datagram_size
                ),
            );
        }
        Err(error) => result_row(scenario, "stream-echo-1mib", "failed", &error),
    }
    middlebox.shutdown();
    let _ = client_thread.join();
}
