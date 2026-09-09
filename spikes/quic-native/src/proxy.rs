//! Deterministic UDP middlebox for two jobs:
//!
//! 1. seeded loss/reorder injection between two real endpoints, and
//! 2. sniffing the client's Initial destination connection ID so the
//!    single-connection `accept` API can be primed for an independent client
//!    that chose its own DCID.
//!
//! std sockets on dedicated threads; the async endpoints under test never see
//! anything but a normal UDP peer.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// xorshift64*: deterministic, seedable, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// True with probability `permille`/1000.
    fn hit(&mut self, permille: u32) -> bool {
        (self.next() % 1000) < u64::from(permille)
    }
}

#[derive(Debug, Default)]
pub struct ProxyStats {
    pub forwarded_c2s: AtomicU64,
    pub forwarded_s2c: AtomicU64,
    pub dropped_c2s: AtomicU64,
    pub dropped_s2c: AtomicU64,
    pub reordered: AtomicU64,
    /// Successfully forwarded server datagrams containing Initial + Handshake.
    pub coalesced_s2c: AtomicU64,
    /// First few observed datagrams as (direction, size) — 'c' = client→server.
    pub wire_log: std::sync::Mutex<Vec<(char, usize)>>,
}

pub struct Proxy {
    /// Address the client should treat as the server.
    pub client_facing: SocketAddr,
    pub stats: Arc<ProxyStats>,
    stop: Arc<AtomicBool>,
    /// First-seen client Initial DCID (long header), when sniffing.
    pub initial_dcid: mpsc::Receiver<Vec<u8>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Proxy {
    /// Forward between an unknown client and `server`, dropping `loss_permille`
    /// of packets per direction and swapping adjacent packets with
    /// `reorder_permille` probability, all from `seed`.
    pub fn spawn(
        server: SocketAddr,
        loss_permille: u32,
        reorder_permille: u32,
        seed: u64,
    ) -> std::io::Result<Proxy> {
        let socket = UdpSocket::bind("127.0.0.1:0")?;
        socket.set_read_timeout(Some(Duration::from_millis(2)))?;
        let client_facing = socket.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(ProxyStats::default());
        let (dcid_tx, dcid_rx) = mpsc::channel();

        let thread_stop = Arc::clone(&stop);
        let thread_stats = Arc::clone(&stats);
        let handle = std::thread::spawn(move || {
            let mut rng = Rng(seed | 1);
            let mut client: Option<SocketAddr> = None;
            let mut dcid_reported = false;
            let mut buffer = [0u8; 65536];
            // One-slot delay line per direction implements deterministic
            // adjacent-packet reordering without unbounded queues.
            let mut held: Option<(Vec<u8>, SocketAddr)> = None;
            while !thread_stop.load(Ordering::Relaxed) {
                match socket.recv_from(&mut buffer) {
                    Ok((len, from)) => {
                        let payload = buffer[..len].to_vec();
                        let from_server = from == server;
                        if !from_server && client.is_none() {
                            client = Some(from);
                        }
                        if !from_server
                            && !dcid_reported
                            && let Some(dcid) = parse_long_header_dcid(&payload)
                        {
                            dcid_reported = true;
                            let _ = dcid_tx.send(dcid);
                        }
                        if let Ok(mut log) = thread_stats.wire_log.lock()
                            && log.len() < 64
                        {
                            log.push((if from_server { 's' } else { 'c' }, len));
                        }
                        let destination = if from_server {
                            match client {
                                Some(addr) => addr,
                                None => continue,
                            }
                        } else {
                            server
                        };
                        if rng.hit(loss_permille) {
                            if from_server {
                                thread_stats.dropped_s2c.fetch_add(1, Ordering::Relaxed);
                            } else {
                                thread_stats.dropped_c2s.fetch_add(1, Ordering::Relaxed);
                            }
                            continue;
                        }
                        if let Some((held_payload, held_destination)) = held.take() {
                            // Deliver current first, held second: a swap.
                            forward(&socket, &payload, destination, server, &thread_stats);
                            forward(
                                &socket,
                                &held_payload,
                                held_destination,
                                server,
                                &thread_stats,
                            );
                            thread_stats.reordered.fetch_add(1, Ordering::Relaxed);
                        } else if rng.hit(reorder_permille) {
                            held = Some((payload, destination));
                            continue;
                        } else {
                            forward(&socket, &payload, destination, server, &thread_stats);
                        }
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        // Flush a held packet rather than delaying it forever.
                        if let Some((held_payload, held_destination)) = held.take() {
                            forward(
                                &socket,
                                &held_payload,
                                held_destination,
                                server,
                                &thread_stats,
                            );
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Proxy {
            client_facing,
            stats,
            stop,
            initial_dcid: dcid_rx,
            handle: Some(handle),
        })
    }

    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn forward(
    socket: &UdpSocket,
    bytes: &[u8],
    destination: SocketAddr,
    server: SocketAddr,
    stats: &ProxyStats,
) {
    if socket.send_to(bytes, destination).ok() != Some(bytes.len()) {
        return;
    }
    if destination == server {
        stats.forwarded_c2s.fetch_add(1, Ordering::Relaxed);
    } else {
        stats.forwarded_s2c.fetch_add(1, Ordering::Relaxed);
        if initial_and_handshake(bytes) {
            stats.coalesced_s2c.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Observe only v1 invariant headers and Length fields (RFC 9000 section 17.2).
/// Never interpret protected packet-number bits or claim to authenticate bytes.
fn initial_and_handshake(mut bytes: &[u8]) -> bool {
    fn varint(bytes: &[u8], offset: &mut usize) -> Option<usize> {
        let first = *bytes.get(*offset)?;
        let width = 1usize << (first >> 6);
        let end = offset.checked_add(width)?;
        let mut value = u64::from(first & 0x3f);
        for byte in bytes.get(*offset + 1..end)? {
            value = (value << 8) | u64::from(*byte);
        }
        *offset = end;
        usize::try_from(value).ok()
    }
    fn packet(bytes: &[u8]) -> Option<(u8, usize)> {
        let first = *bytes.first()?;
        if first & 0xc0 != 0xc0 || bytes.get(1..5)? != [0, 0, 0, 1] {
            return None;
        }
        let kind = (first >> 4) & 3;
        if kind == 3 {
            return None;
        } // Retry has no Length field.
        let dcid = usize::from(*bytes.get(5)?);
        if dcid > 20 {
            return None;
        }
        let scid = usize::from(*bytes.get(6 + dcid)?);
        if scid > 20 {
            return None;
        }
        let mut offset = 7 + dcid + scid;
        if kind == 0 {
            let token = varint(bytes, &mut offset)?;
            offset = offset.checked_add(token)?;
        }
        let length = varint(bytes, &mut offset)?;
        let end = offset.checked_add(length)?;
        // Protected PN plus AEAD tag; do not read the protected PN length.
        (length >= 17 && end <= bytes.len()).then_some((kind, end))
    }
    let mut initial = false;
    // Bound work even on a malformed or adversarial datagram.
    for _ in 0..8 {
        let Some((kind, end)) = packet(bytes) else {
            return false;
        };
        if kind == 2 && initial {
            return true;
        }
        initial |= kind == 0;
        bytes = &bytes[end..];
    }
    false
}

#[test]
fn coalesced_observer_checks_visible_lengths_without_reading_protected_bits() {
    // Prefix-only parser fixtures, not encrypted packets or interoperability proof.
    let initial = [vec![0xcf, 0, 0, 0, 1, 0, 0, 0, 17], vec![0; 17]].concat();
    let handshake = [vec![0xef, 0, 0, 0, 1, 0, 0, 17], vec![0; 17]].concat();
    assert!(!initial_and_handshake(&initial));
    assert!(!initial_and_handshake(&handshake));
    let flight = [initial.clone(), handshake.clone()].concat();
    assert!(initial_and_handshake(&flight));
    for end in 0..flight.len() {
        assert!(!initial_and_handshake(&flight[..end]));
    }
    assert!(!initial_and_handshake(&[handshake, initial].concat()));
    let mut wrong_version = flight.clone();
    wrong_version[4] = 2;
    assert!(!initial_and_handshake(&wrong_version));
}

/// Destination connection ID of a QUIC long-header packet (RFC 8999 §5.1).
pub fn parse_long_header_dcid(packet: &[u8]) -> Option<Vec<u8>> {
    if packet.len() < 7 || packet[0] & 0x80 == 0 {
        return None;
    }
    let dcid_len = usize::from(*packet.get(5)?);
    if dcid_len > 20 {
        return None;
    }
    packet.get(6..6 + dcid_len).map(<[u8]>::to_vec)
}
