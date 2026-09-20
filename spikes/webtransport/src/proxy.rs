//! UDP proxy with Initial DCID sniffing.
//! Allows browser clients (Chrome) with randomly generated Initial DCIDs
//! to be admitted by NativeQuicUdpConnection::accept.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

pub struct Proxy {
    pub client_facing: SocketAddr,
    stop: Arc<AtomicBool>,
    pub initial_dcid: mpsc::Receiver<Vec<u8>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Proxy {
    pub fn spawn(server: SocketAddr) -> std::io::Result<Proxy> {
        let socket = UdpSocket::bind("127.0.0.1:0")?;
        socket.set_read_timeout(Some(Duration::from_millis(5)))?;
        let client_facing = socket.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (dcid_tx, dcid_rx) = mpsc::channel();

        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut client: Option<SocketAddr> = None;
            let mut dcid_reported = false;
            let mut buffer = [0u8; 65536];

            while !thread_stop.load(Ordering::Relaxed) {
                match socket.recv_from(&mut buffer) {
                    Ok((len, from)) => {
                        let payload = &buffer[..len];
                        let from_server = from == server;
                        if !from_server && client.is_none() {
                            client = Some(from);
                        }
                        if !from_server
                            && !dcid_reported
                            && let Some(dcid) = parse_long_header_dcid(payload)
                        {
                            dcid_reported = true;
                            let _ = dcid_tx.send(dcid);
                        }
                        let destination = if from_server {
                            match client {
                                Some(addr) => addr,
                                None => continue,
                            }
                        } else {
                            server
                        };
                        let _ = socket.send_to(payload, destination);
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => break,
                }
            }
        });

        Ok(Proxy {
            client_facing,
            stop,
            initial_dcid: dcid_rx,
            handle: Some(handle),
        })
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn parse_long_header_dcid(bytes: &[u8]) -> Option<Vec<u8>> {
    let first = *bytes.first()?;
    // Long header: high bit set, fixed bit set
    if first & 0xc0 != 0xc0 || bytes.len() < 7 {
        return None;
    }
    // Version: 4 bytes (bytes 1..5)
    let dcid_len = usize::from(*bytes.get(5)?);
    if dcid_len == 0 || dcid_len > 20 || bytes.len() < 6 + dcid_len {
        return None;
    }
    Some(bytes[6..6 + dcid_len].to_vec())
}
