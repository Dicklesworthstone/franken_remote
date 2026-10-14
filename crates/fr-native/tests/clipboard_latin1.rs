//! Actual X11 properties exchanged with an independent Python/Xlib peer.
//! Native charset interoperability only, not an authenticated remote session.
#![cfg(all(target_os = "linux", feature = "linux-clipboard"))]
#![forbid(unsafe_code)]
use fr_core::limits::{LimitOverrides, ProtocolLimits};
use fr_native::clipboard::{ReadError, ReadText, X11Clipboard};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, BufWriter, Write},
    process::{Child, Command, Stdio},
    sync::{Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
static SERIAL: Mutex<()> = Mutex::new(());
const PEER: &str = include_str!("clipboard_latin1/peer.py");

struct Peer {
    child: Child,
    lines: mpsc::Receiver<String>,
    reader: Option<thread::JoinHandle<()>>,
}
impl Peer {
    fn start(display: &str, mut settings: Value) -> Self {
        settings["display"] = json!(display);
        let mut child = Command::new("python3")
            .args(["-u", "-c", PEER])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        {
            let mut buffered = BufWriter::new(&mut stdin);
            serde_json::to_writer(&mut buffered, &settings).unwrap();
            buffered.write_all(b"\n").unwrap();
            buffered.flush().unwrap();
        }
        drop(stdin);
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = mpsc::sync_channel(16);
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if send.try_send(line).is_err() {
                    break;
                }
            }
        });
        let peer = Self {
            child,
            lines,
            reader: Some(reader),
        };
        assert_eq!(
            peer.lines.recv_timeout(Duration::from_secs(3)).unwrap(),
            "READY"
        );
        peer
    }
    fn requests(&self) -> Vec<String> {
        self.lines.try_iter().collect()
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
    }
}
fn display() -> Option<String> {
    let display = std::env::var("DISPLAY").ok();
    assert!(
        display.is_some() || std::env::var_os("FR_NATIVE_CLIPBOARD_REQUIRED").is_none(),
        "required native clipboard tests need DISPLAY and Python/Xlib"
    );
    display
}
fn open(display: &str, watched: bool, limits: &ProtocolLimits) -> X11Clipboard {
    let mut reader = X11Clipboard::open(display, limits, true).unwrap();
    if watched {
        reader.start_watching().unwrap();
    }
    reader
}
fn finish(reader: &mut X11Clipboard) -> Result<ReadText, ReadError> {
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        if let Some(text) = reader.poll_read()? {
            return Ok(text);
        }
        assert!(
            Instant::now() < until,
            "native operation exceeded its own lifetime"
        );
        thread::sleep(Duration::from_micros(100));
    }
}
#[test]
fn latin1_only_native_owners_deliver_exact_text_with_or_without_watching() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let alphabet: Vec<_> = (*b"\t\n")
        .into_iter()
        .chain(32..=126)
        .chain(160..=255)
        .collect();
    for bytes in [Vec::new(), b"caf\xe9 and \xa3".to_vec(), alphabet] {
        for watched in [false, true] {
            let peer = Peer::start(&display, json!({"body": bytes}));
            let mut reader = open(&display, watched, &ProtocolLimits::ABSOLUTE);
            reader.begin_read().unwrap();
            let text = finish(&mut reader).unwrap();
            assert_eq!(
                text.as_str(),
                bytes.iter().copied().map(char::from).collect::<String>()
            );
            assert_eq!(text.origin(), None);
            assert_eq!(format!("{text:?}"), "ClipboardReadText([redacted])");
            assert_eq!(peer.requests(), ["UTF8", "STRING"]);
        }
    }
}
#[test]
fn utf8_is_preferred_and_malformed_utf8_is_never_reinterpreted_as_latin1() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    for malformed in [false, true] {
        let bytes: &[u8] = if malformed {
            &[0xe9]
        } else {
            "fresh 🦀".as_bytes()
        };
        let peer = Peer::start(&display, json!({"body": bytes, "utf8_mode": "data"}));
        let mut reader = open(&display, true, &ProtocolLimits::ABSOLUTE);
        reader.begin_read().unwrap();
        let result = finish(&mut reader);
        if malformed {
            assert!(matches!(result, Err(ReadError::InvalidUtf8)));
        } else {
            assert_eq!(result.unwrap().as_str(), "fresh 🦀");
        }
        assert_eq!(peer.requests(), ["UTF8"]);
    }
}
#[test]
fn native_latin1_expansion_respects_the_negotiated_utf8_byte_limit() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_clipboard_item_bytes: Some(1024),
        ..Default::default()
    })
    .unwrap();
    for incremental in [false, true] {
        for size in [512, 513] {
            let peer = Peer::start(
                &display,
                json!({"body": vec![0xe9; size], "incremental": incremental, "chunk": 257}),
            );
            let mut reader = open(&display, true, &limits);
            reader.begin_read().unwrap();
            let result = finish(&mut reader);
            if size == 512 {
                assert_eq!(result.unwrap().as_str(), "é".repeat(size));
            } else {
                assert!(matches!(result, Err(ReadError::Limit)));
            }
            assert_eq!(peer.requests(), ["UTF8", "STRING"]);
            assert!(matches!(reader.poll_read(), Err(ReadError::NotReading)));
        }
    }
}

#[test]
fn incremental_conversion_crosses_native_read_turns_without_counting_expansion_as_source_bytes() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    for hint in [None, Some(524_288)] {
        let peer = Peer::start(
            &display,
            json!({
                "body": vec![0xe9; 524_288], "incremental": true, "chunk": 65_537, "hint": hint
            }),
        );
        let mut reader = open(&display, true, &ProtocolLimits::ABSOLUTE);
        reader.begin_read().unwrap();
        assert_eq!(finish(&mut reader).unwrap().as_str(), "é".repeat(524_288));
        assert_eq!(peer.requests(), ["UTF8", "STRING"]);
    }
    // 1024 UTF-8 bytes must not satisfy a false 700-NATIVE-byte lower bound.
    let _peer = Peer::start(
        &display,
        json!({
            "body": vec![0xe9; 512], "incremental": true, "hint": 700
        }),
    );
    let mut reader = open(&display, true, &ProtocolLimits::ABSOLUTE);
    reader.begin_read().unwrap();
    assert!(matches!(finish(&mut reader), Err(ReadError::Malformed)));
}

#[test]
fn native_string_refusals_bad_types_and_non_text_controls_never_produce_a_copy() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    for (settings, expected) in [
        (json!({"string_mode": "refuse"}), ReadError::Unsupported),
        (
            json!({"body": [233], "string_mode": "wrong_type"}),
            ReadError::Malformed,
        ),
        (json!({"body": [97, 0, 98]}), ReadError::Unsupported),
        (
            json!({"body": [97, 128, 98], "incremental": true}),
            ReadError::Unsupported,
        ),
    ] {
        let peer = Peer::start(&display, settings);
        let mut reader = open(&display, true, &ProtocolLimits::ABSOLUTE);
        reader.begin_read().unwrap();
        assert!(matches!(finish(&mut reader), Err(error) if error == expected));
        assert_eq!(peer.requests(), ["UTF8", "STRING"]);
        assert!(matches!(reader.poll_read(), Err(ReadError::NotReading)));
    }
}

fn reach_string_request(peer: &Peer, reader: &mut X11Clipboard) {
    let until = Instant::now() + Duration::from_secs(1);
    let mut requests = Vec::new();
    loop {
        assert!(reader.poll_read().unwrap().is_none());
        requests.extend(peer.requests());
        if requests.iter().any(|r| r == "STRING") {
            break;
        }
        assert!(Instant::now() < until, "no native STRING request");
        thread::sleep(Duration::from_micros(100));
    }
    assert_eq!(requests, ["UTF8", "STRING"]);
}

#[test]
fn owner_changes_and_replaced_watchers_fence_a_pending_charset_conversion() {
    use fr_core::clipboard::{ClipboardSink, Endpoint, Publication, Stamp};
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    for replace_owner in [false, true] {
        let peer = Peer::start(&display, json!({"string_mode": "silent"}));
        let mut reader = open(&display, true, &ProtocolLimits::ABSOLUTE);
        reader.begin_read().unwrap();
        reach_string_request(&peer, &mut reader);
        let mut replacement = open(&display, false, &ProtocolLimits::ABSOLUTE);
        if replace_owner {
            let stamp = Stamp {
                id: 1,
                source: Endpoint::Host,
                sequence: 1,
            };
            replacement.prepare("new native copy", stamp).unwrap();
            assert_eq!(
                replacement.publish("new native copy", stamp),
                Publication::SubmittedToOs
            );
        } else {
            reader.stop_watching().unwrap();
            reader.start_watching().unwrap();
        }
        assert!(matches!(finish(&mut reader), Err(ReadError::LocalChanged)));
        assert!(matches!(reader.poll_read(), Err(ReadError::NotReading)));
    }
}

#[test]
fn changing_encoding_keeps_the_original_absolute_deadline() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let peer = Peer::start(&display, json!({"string_mode": "silent"}));
    let mut reader = open(&display, true, &ProtocolLimits::ABSOLUTE);
    reader.begin_read().unwrap();
    // Leave the first clock event unserviced. Native work is still bounded by
    // begin_read, not by the delayed first request or charset selection.
    thread::sleep(Duration::from_millis(2_600));
    reach_string_request(&peer, &mut reader);
    thread::sleep(Duration::from_millis(500));
    assert!(matches!(reader.poll_read(), Err(ReadError::Expired)));
    assert!(matches!(reader.poll_read(), Err(ReadError::NotReading)));
}

#[path = "clipboard_latin1/automatic.rs"]
mod automatic;
#[path = "clipboard_latin1/publication.rs"]
mod publication;
