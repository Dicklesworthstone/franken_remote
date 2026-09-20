//! Bounded WSS fallback profile demonstration.
//! Proves:
//! 1. Separately-bound channels (control, video, audio).
//! 2. Receiver-granted outstanding-byte credit (bufferedAmount only reports outbound buffering).
//! 3. Stale-generation fencing on channel replacement.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WssCreditReport {
    pub channels_opened: Vec<String>,
    pub initial_credit_granted: usize,
    pub bytes_sent_before_stall: usize,
    pub backpressure_observed: bool,
    pub bytes_sent_after_topup: usize,
    pub stale_generation_rejected: bool,
    pub active_generation: u32,
}

pub struct WssServer {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    pub report: Arc<Mutex<WssCreditReport>>,
}

impl WssServer {
    pub fn spawn() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));

        let report = Arc::new(Mutex::new(WssCreditReport {
            channels_opened: Vec::new(),
            initial_credit_granted: 0,
            bytes_sent_before_stall: 0,
            backpressure_observed: false,
            bytes_sent_after_topup: 0,
            stale_generation_rejected: false,
            active_generation: 1,
        }));

        let thread_stop = Arc::clone(&stop);
        let thread_report = Arc::clone(&report);

        let handle = thread::spawn(move || {
            let active_gen = Arc::new(AtomicU32::new(1));

            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let rep = Arc::clone(&thread_report);
                        let cur_gen = Arc::clone(&active_gen);
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                        thread::spawn(move || {
                            handle_client(stream, rep, cur_gen);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            addr,
            stop,
            handle: Some(handle),
            report,
        })
    }
}

impl Drop for WssServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_client(
    mut stream: TcpStream,
    report: Arc<Mutex<WssCreditReport>>,
    active_gen: Arc<AtomicU32>,
) {
    let mut header_buf = [0u8; 4096];
    let n = match stream.read(&mut header_buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let request = String::from_utf8_lossy(&header_buf[..n]);

    // Parse channel and gen from GET /channel?role=<role>&gen=<gen>
    let mut channel_role = "unknown".to_string();
    let mut requested_gen = 1u32;
    if let Some(first_line) = request.lines().next() {
        if let Some(path) = first_line.split_whitespace().nth(1) {
            if let Some(query) = path.split('?').nth(1) {
                for pair in query.split('&') {
                    let mut parts = pair.split('=');
                    if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
                        if k == "role" {
                            channel_role = v.to_string();
                        } else if k == "gen" {
                            requested_gen = v.parse().unwrap_or(1);
                        }
                    }
                }
            }
        }
    }

    // Extract Sec-WebSocket-Key
    let key = request
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key:"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| s.trim())
        .unwrap_or("");

    if key.is_empty() {
        let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
        return;
    }

    // Compute accept key
    let accept_val = compute_accept_key(key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        accept_val
    );
    if stream.write_all(response.as_bytes()).is_err() {
        return;
    }

    {
        let mut rep = report.lock().unwrap();
        rep.channels_opened.push(channel_role.clone());
        if requested_gen > rep.active_generation {
            rep.active_generation = requested_gen;
            active_gen.store(requested_gen, Ordering::SeqCst);
        }
    }

    let mut available_credit = 0usize;
    let mut frame_reader = WsFrameReader::new();

    loop {
        // Read incoming WebSocket frames
        let mut buf = [0u8; 4096];
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                frame_reader.feed(&buf[..n]);
                while let Some(msg) = frame_reader.next_message() {
                    if let Ok(text) = std::str::from_utf8(&msg) {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                            let msg_type = v["type"].as_str().unwrap_or("");
                            let msg_gen = v["generation"].as_u64().unwrap_or(1) as u32;

                            // Stale generation check
                            let cur_gen = active_gen.load(Ordering::SeqCst);
                            if msg_gen < cur_gen {
                                let mut rep = report.lock().unwrap();
                                rep.stale_generation_rejected = true;
                                let refusal = serde_json::json!({
                                    "type": "refused",
                                    "reason": "stale_generation",
                                    "expected": cur_gen,
                                    "received": msg_gen
                                });
                                let _ = send_ws_text(&mut stream, &refusal.to_string());
                                continue;
                            }

                            if msg_type == "grant_credit" {
                                let grant = v["bytes"].as_u64().unwrap_or(0) as usize;
                                available_credit += grant;

                                let mut rep = report.lock().unwrap();
                                if rep.initial_credit_granted == 0 {
                                    rep.initial_credit_granted = grant;
                                }

                                // Send media records under credit
                                const RECORD_SIZE: usize = 1024;
                                while available_credit >= RECORD_SIZE {
                                    let record = vec![0xaa; RECORD_SIZE];
                                    if send_ws_binary(&mut stream, &record).is_err() {
                                        return;
                                    }
                                    available_credit -= RECORD_SIZE;

                                    if rep.backpressure_observed {
                                        rep.bytes_sent_after_topup += RECORD_SIZE;
                                    } else {
                                        rep.bytes_sent_before_stall += RECORD_SIZE;
                                    }
                                }

                                if available_credit < RECORD_SIZE {
                                    rep.backpressure_observed = true;
                                }
                            }
                        }
                    }
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Idle
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

fn compute_accept_key(key: &str) -> String {
    // Standard WebSocket GUID: 258EAFA5-E914-47DA-95CA-C5AB0DC85B11
    // Note: RFC 6455 specifies SHA-1, but for testing handshake with standard SHA1:
    let combined = format!("{}258EAFA5-E914-47DA-95CA-C5AB0DC85B11", key);
    // Use simple SHA-1 implementation
    let hash = sha1_hash(combined.as_bytes());
    base64_encode(&hash)
}

fn sha1_hash(data: &[u8]) -> [u8; 20] {
    // Pure standard SHA-1 implementation
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    let ml = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..(i + 1) * 4].try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;
        let mut e = h4;

        for i in 0..80 {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < input.len() {
        let b0 = input[i];
        let b1 = if i + 1 < input.len() { input[i + 1] } else { 0 };
        let b2 = if i + 2 < input.len() { input[i + 2] } else { 0 };

        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 3) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < input.len() {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < input.len() {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

fn send_ws_text(stream: &mut TcpStream, text: &str) -> std::io::Result<()> {
    let bytes = text.as_bytes();
    let mut frame = Vec::new();
    frame.push(0x81); // Text opcode
    encode_len(bytes.len(), &mut frame);
    frame.extend_from_slice(bytes);
    stream.write_all(&frame)
}

fn send_ws_binary(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    let mut frame = Vec::new();
    frame.push(0x82); // Binary opcode
    encode_len(bytes.len(), &mut frame);
    frame.extend_from_slice(bytes);
    stream.write_all(&frame)
}

fn encode_len(len: usize, frame: &mut Vec<u8>) {
    if len <= 125 {
        frame.push(len as u8);
    } else if len <= 65535 {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
}

struct WsFrameReader {
    buffer: Vec<u8>,
}

impl WsFrameReader {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }
    fn feed(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }
    fn next_message(&mut self) -> Option<Vec<u8>> {
        if self.buffer.len() < 2 {
            return None;
        }
        let b1 = self.buffer[1];
        let masked = (b1 & 0x80) != 0;
        let mut payload_len = (b1 & 0x7f) as usize;
        let mut header_len = 2;

        if payload_len == 126 {
            if self.buffer.len() < 4 {
                return None;
            }
            payload_len = u16::from_be_bytes([self.buffer[2], self.buffer[3]]) as usize;
            header_len = 4;
        } else if payload_len == 127 {
            if self.buffer.len() < 10 {
                return None;
            }
            payload_len = u64::from_be_bytes(self.buffer[2..10].try_into().unwrap()) as usize;
            header_len = 10;
        }

        let mask_len = if masked { 4 } else { 0 };
        if self.buffer.len() < header_len + mask_len + payload_len {
            return None;
        }

        let mut payload = self.buffer[header_len + mask_len..header_len + mask_len + payload_len].to_vec();
        if masked {
            let mask = &self.buffer[header_len..header_len + 4];
            for (i, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[i % 4];
            }
        }

        self.buffer.drain(..header_len + mask_len + payload_len);
        Some(payload)
    }
}
