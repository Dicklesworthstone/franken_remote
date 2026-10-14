//! Independent owners without TIMESTAMP: real Xlib/XCB, not fake native reads.
use super::*;
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::{fd::OwnedFd, unix::net::UnixStream},
    process::{Child, ChildStdin, Command, Stdio},
};

struct Peer {
    process: Child,
    input: ChildStdin,
    output: BufReader<UnixStream>,
}
impl Peer {
    fn new(display: &str, mode: &str) -> Self {
        let (reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let mut process = Command::new("python3")
            .args(["-u", "-c", include_str!("legacy_owner.py"), mode])
            .env("DISPLAY", display)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = process.stdin.take().unwrap();
        let mut peer = Self {
            process,
            input,
            output: BufReader::new(reader),
        };
        assert_eq!(peer.line(), "READY");
        peer
    }
    fn line(&mut self) -> String {
        let mut line = String::new();
        self.output.by_ref().take(64).read_line(&mut line).unwrap();
        assert!(line.ends_with('\n'));
        line.trim().to_owned()
    }
    fn copy(&mut self, text: &str, same_time: bool) {
        use std::fmt::Write as _;
        let mut hex = String::new();
        for byte in text.bytes() {
            write!(&mut hex, "{byte:02x}").unwrap();
        }
        let command = if same_time { "same" } else { "own" };
        writeln!(
            self.input,
            "{command} {}",
            if hex.is_empty() { "-" } else { &hex }
        )
        .unwrap();
        assert_eq!(self.line(), "OWNED");
    }
    fn requests(&mut self) -> usize {
        writeln!(self.input, "stats").unwrap();
        self.line().parse().unwrap()
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.process.kill();
        self.process
            .wait()
            .expect("reap independent clipboard peer");
    }
}
fn settled(reader: &mut X11Clipboard) {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if reader.poll_change().unwrap().is_some() {
            return;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_micros(100));
    }
}
fn complete(reader: &mut X11Clipboard) -> Result<ReadText, ReadError> {
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        if let Some(text) = reader.poll_read()? {
            return Ok(text);
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_micros(100));
    }
}

#[test]
fn watched_owners_without_timestamp_deliver_empty_unicode_and_incr() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    for mode in ["refuse", "text"] {
        let mut peer = Peer::new(&display, mode);
        let mut reader = open(&display);
        reader.start_watching().unwrap();
        settled(&mut reader);
        for text in [
            String::new(),
            "copy λ 🦀\n\0tail".to_owned(),
            "🦀".repeat(32_768),
        ] {
            peer.copy(&text, false);
            settled(&mut reader);
            reader.begin_read().unwrap();
            let observed = complete(&mut reader).unwrap();
            assert_eq!(observed.as_str(), text);
            assert_eq!(observed.origin(), None);
        }
        // A watched read asks only for UTF8, never TIMESTAMP in either direction.
        assert_eq!(peer.requests(), 3);
    }
}

#[test]
fn without_a_watch_timestamp_less_owners_still_refuse() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut peer = Peer::new(&display, "refuse");
    peer.copy("no server subscription", false);
    let mut reader = open(&display);
    reader.begin_read().unwrap();
    assert_eq!(complete(&mut reader).unwrap_err(), ReadError::Unsupported);
}

#[test]
fn same_owner_same_timestamp_reselection_fences_a_watched_read() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut peer = Peer::new(&display, "text");
    peer.copy(&"old ".repeat(32_768), false);
    let mut reader = open(&display);
    reader.start_watching().unwrap();
    settled(&mut reader);
    reader.begin_read().unwrap();
    // Submit UTF8, let the external owner respond, but do not finish its INCR.
    assert!(reader.poll_read().unwrap().is_none());
    let until = Instant::now() + Duration::from_secs(2);
    while peer.requests() == 0 {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_micros(100));
    }
    peer.copy("new selection at identical X timestamp", true);
    assert_eq!(complete(&mut reader).unwrap_err(), ReadError::LocalChanged);
    settled(&mut reader);
    reader.begin_read().unwrap();
    assert_eq!(
        complete(&mut reader).unwrap().as_str(),
        "new selection at identical X timestamp"
    );
}

#[test]
fn ending_or_replacing_the_subscription_never_downgrades_an_active_read() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut peer = Peer::new(&display, "text");
    peer.copy("old epoch", false);
    for restart in [false, true] {
        let mut reader = open(&display);
        reader.start_watching().unwrap();
        settled(&mut reader);
        reader.begin_read().unwrap();
        reader.stop_watching().unwrap();
        if restart {
            reader.start_watching().unwrap();
        }
        assert_eq!(complete(&mut reader).unwrap_err(), ReadError::LocalChanged);
    }
}
