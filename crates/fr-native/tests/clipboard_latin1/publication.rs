//! Actual XCB publication consumed by an independent Python/Xlib requestor.
//! Native selection is not a paste action or live remote-session qualification.
use super::*;
use fr_core::clipboard::{ClipboardSink, Endpoint, Publication, Stamp};
use std::process::ChildStdin;
const CONSUMER: &str = include_str!("consumer.py");

pub(super) struct Consumer {
    child: Child,
    input: ChildStdin,
    lines: mpsc::Receiver<String>,
    reader: Option<thread::JoinHandle<()>>,
}
impl Consumer {
    pub(super) fn start(display: &str, mut settings: Value) -> Self {
        settings["display"] = json!(display);
        let mut child = Command::new("python3")
            .args(["-u", "-c", CONSUMER])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        {
            let mut buffered = BufWriter::new(&mut input);
            serde_json::to_writer(&mut buffered, &settings).unwrap();
            buffered.write_all(b"\n").unwrap();
            buffered.flush().unwrap();
        }
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
        let consumer = Self {
            child,
            input,
            lines,
            reader: Some(reader),
        };
        assert_eq!(
            consumer.lines.recv_timeout(Duration::from_secs(3)).unwrap(),
            "READY"
        );
        consumer
    }
    fn wait(&self, source: &mut X11Clipboard, expected: &str) {
        self.wait_driven(
            || {
                source.pump().unwrap();
            },
            expected,
        );
    }
    pub(super) fn wait_driven(&self, mut drive: impl FnMut(), expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            drive();
            match self.lines.try_recv() {
                Ok(actual) => {
                    assert_eq!(actual, expected);
                    return;
                }
                Err(mpsc::TryRecvError::Disconnected) => panic!("native requestor failed"),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(
                Instant::now() < deadline,
                "native requestor made no progress"
            );
            thread::sleep(Duration::from_micros(100));
        }
    }
    fn go(&mut self) {
        self.input.write_all(b"GO\n").unwrap();
        self.input.flush().unwrap();
    }
    pub(super) fn done(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "independent requestor failed");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "requestor did not exit after completion"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Drop for Consumer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
    }
}
fn publish(source: &mut X11Clipboard, text: &str, sequence: u64) {
    let stamp = Stamp {
        id: u128::from(sequence),
        source: Endpoint::Host,
        sequence,
    };
    source.prepare(text, stamp).unwrap();
    assert_eq!(source.publish(text, stamp), Publication::SubmittedToOs);
}
fn drained(source: &mut X11Clipboard) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while source.active_readers() != 0 {
        source.pump().unwrap();
        assert!(
            Instant::now() < deadline,
            "native reader retained after completion"
        );
        thread::sleep(Duration::from_micros(100));
    }
}

#[test]
fn native_string_publication_preserves_empty_graphics_and_full_size_text() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display, false, &ProtocolLimits::ABSOLUTE);
    let alphabet: Vec<_> = (*b"\t\n")
        .into_iter()
        .chain(32..=126)
        .chain(160..=255)
        .collect();
    for (index, bytes) in [
        Vec::new(),
        alphabet,
        vec![0xe9; 16_384],
        vec![0xa3; 16_385],
        vec![0xff; 524_288],
    ]
    .into_iter()
    .enumerate()
    {
        let text: String = bytes.iter().copied().map(char::from).collect();
        publish(&mut source, &text, u64::try_from(index).unwrap() + 1);
        let mut consumer = Consumer::start(&display, json!({"body": bytes}));
        consumer.wait(&mut source, "TARGETS_STRING");
        if bytes.len() > 16_384 {
            consumer.wait(&mut source, "INCR");
        }
        consumer.wait(&mut source, "MATCH");
        consumer.done();
        drained(&mut source);
    }
}

#[test]
fn unrepresentable_selections_are_not_advertised_or_lossily_substituted() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display, false, &ProtocolLimits::ABSOLUTE);
    for (index, text) in ["remote 🦀", "copy\0tail", "c1\u{85}tail", "del\u{7f}"]
        .into_iter()
        .enumerate()
    {
        publish(&mut source, text, u64::try_from(index).unwrap() + 1);
        let mut legacy = Consumer::start(&display, json!({"refused": true}));
        legacy.wait(&mut source, "TARGETS_UTF8_ONLY");
        legacy.wait(&mut source, "REFUSED");
        legacy.done();
        let mut unicode = Consumer::start(
            &display,
            json!({"target": "UTF8_STRING", "body": text.as_bytes()}),
        );
        unicode.wait(&mut source, "TARGETS_UTF8_ONLY");
        unicode.wait(&mut source, "MATCH");
        unicode.done();
        assert_eq!(source.active_readers(), 0);
    }
}

#[test]
fn outstanding_string_and_utf8_readers_retain_their_original_selection_and_encoding() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display, false, &ProtocolLimits::ABSOLUTE);
    let text = "aé£\n".repeat(8193);
    publish(&mut source, &text, 1);
    let latin: Vec<_> = text
        .chars()
        .map(|ch| u8::try_from(u32::from(ch)).unwrap())
        .collect();
    let mut readers = Vec::new();
    for target in ["STRING", "UTF8_STRING", "STRING", "UTF8_STRING"] {
        let body = if target == "STRING" {
            latin.as_slice()
        } else {
            text.as_bytes()
        };
        let reader = Consumer::start(
            &display,
            json!({"target": target, "body": body, "pause_header": true, "pause_start": true}),
        );
        readers.push(reader);
    }
    let mut excess = Consumer::start(&display, json!({"refused": true, "pause_start": true}));
    // Interpreter startup and fixture-JSON parsing must finish BEFORE any
    // native reader's original three-second lifetime begins. Otherwise setup
    // alone can expire a correct reader before the capacity assertion.
    for reader in &mut readers {
        reader.go();
    }
    for reader in &readers {
        reader.wait(&mut source, "TARGETS_STRING");
        reader.wait(&mut source, "INCR");
    }
    assert_eq!(source.active_readers(), 4);
    excess.go();
    excess.wait(&mut source, "TARGETS_STRING");
    excess.wait(&mut source, "REFUSED");
    excess.done();
    publish(&mut source, "new 🦀 selection", 2);
    for reader in &mut readers {
        reader.go();
        reader.wait(&mut source, "MATCH");
        reader.done();
    }
    drained(&mut source);
    let mut current = Consumer::start(
        &display,
        json!({"target": "UTF8_STRING", "body": "new 🦀 selection".as_bytes()}),
    );
    current.wait(&mut source, "TARGETS_UTF8_ONLY");
    current.wait(&mut source, "MATCH");
    current.done();
}

#[test]
fn an_expired_string_reader_never_receives_a_false_completion_terminator() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut source = open(&display, false, &ProtocolLimits::ABSOLUTE);
    publish(&mut source, &"é".repeat(32_768), 1);
    let mut reader = Consumer::start(
        &display,
        json!({"body": vec![0xe9; 32_768], "pause_after": 1, "expect_timeout": true}),
    );
    reader.wait(&mut source, "TARGETS_STRING");
    reader.wait(&mut source, "INCR");
    reader.wait(&mut source, "PAUSED");
    thread::sleep(Duration::from_millis(3_020));
    source.pump().unwrap();
    assert_eq!(source.active_readers(), 0);
    reader.go();
    reader.wait(&mut source, "TIMED_OUT_PARTIAL");
    reader.done();
}
