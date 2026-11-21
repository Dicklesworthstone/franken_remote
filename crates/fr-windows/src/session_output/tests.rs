use super::*;
use std::{
    io::Cursor,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

const REPORT: &[u8] = b"{\"status\":\"ended\",\"end\":\"ok\",\"drawn\":12}";

struct TempFile(PathBuf);
impl TempFile {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-session-output-{}-{}.log",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        File::options()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap();
        Self(path)
    }
}
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn huge_existing_log_reads_only_its_suffix_and_retains_last_complete_line() {
    let path = TempFile::new();
    let mut original = File::options().write(true).open(&path.0).unwrap();
    let size = 32 * LOG_LIMIT as u64;
    original.set_len(size).unwrap();
    let tail = b"\nlast prior session\n";
    original
        .seek(SeekFrom::Start(size - tail.len() as u64))
        .unwrap();
    original.write_all(tail).unwrap();
    drop(original);
    let mut log = CappedLog::open(&path.0).unwrap();
    assert!(std::fs::metadata(&path.0).unwrap().len() <= LOG_LIMIT as u64);
    log.append(b"new session\n").unwrap();
    let text = std::fs::read(&path.0).unwrap();
    assert!(text.starts_with(OMITTED_LOG));
    assert!(text.ends_with(b"last prior session\nnew session\n"));
}

#[test]
fn active_session_log_stays_bounded_and_keeps_terminal_report() {
    let path = TempFile::new();
    let mut log = CappedLog::open(&path.0).unwrap();
    let mut line = vec![b'x'; STATUS_LINE_LIMIT];
    line.push(b'\n');
    for _ in 0..2048 {
        log.append(&line).unwrap();
        assert!(std::fs::metadata(&path.0).unwrap().len() <= LOG_LIMIT as u64);
    }
    log.append(REPORT).unwrap();
    log.append(b"\n").unwrap();
    let text = std::fs::read(&path.0).unwrap();
    assert!(text.starts_with(OMITTED_LOG));
    assert!(text.ends_with(&[REPORT, b"\n"].concat()));
}

#[test]
fn oversized_append_has_explicit_marker_and_never_exceeds_cap() {
    let path = TempFile::new();
    let mut log = CappedLog::open(&path.0).unwrap();
    log.append(&vec![b'x'; LOG_LIMIT + 1]).unwrap();
    let bytes = std::fs::read(&path.0).unwrap();
    assert_eq!(bytes.len(), LOG_LIMIT);
    assert!(bytes.starts_with(OMITTED_ENTRY));
    log.append(b"\nnew complete line\n").unwrap();
    assert!(std::fs::metadata(&path.0).unwrap().len() <= LOG_LIMIT as u64);
}

#[test]
fn logging_failure_is_explicit_and_does_not_stop_status_draining() {
    let path = TempFile::new();
    let file = File::open(&path.0).unwrap();
    let mut log = CappedLog { file };
    let mut failures = 0;
    let report = drain_status(Cursor::new(b"first\nsecond\nthird\n"), |line, _| {
        if log.append(line).is_err() {
            failures += 1;
        }
    });
    assert_eq!(failures, 3);
    assert_eq!(report.lines, 3);
    assert!(report.complete());
}

#[test]
fn another_log_writer_is_a_nonblocking_typed_refusal() {
    let path = TempFile::new();
    let owner = File::options()
        .read(true)
        .write(true)
        .open(&path.0)
        .unwrap();
    let mut log = CappedLog::open(&path.0).unwrap();
    owner.lock().unwrap();
    assert_eq!(
        log.append(b"must not wait\n").unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    owner.unlock().unwrap();
    log.append(b"available again\n").unwrap();
    assert_eq!(std::fs::read(&path.0).unwrap(), b"available again\n");
}
