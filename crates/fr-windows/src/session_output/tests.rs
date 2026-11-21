use super::*;
use std::{
    io::Cursor,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

const REPORT: &[u8] = b"{\"status\":\"ended\",\"end\":\"ok\",\"drawn\":12}";

/// A large stream whose content is generated incrementally, not allocated.
struct Repeated {
    left: usize,
    byte: u8,
}
impl Read for Repeated {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let count = out.len().min(self.left);
        out[..count].fill(self.byte);
        self.left -= count;
        Ok(count)
    }
}

#[test]
fn unterminated_status_flood_is_drained_with_one_bounded_prefix() {
    let size = 16 * 1024 * 1024;
    let mut consumed = 0;
    let report = drain_status(
        Repeated {
            left: size,
            byte: b'x',
        },
        |bytes, truncated| {
            consumed += 1;
            assert_eq!(bytes.len(), STATUS_LINE_LIMIT);
            assert!(truncated);
        },
    );
    assert_eq!(consumed, 1);
    assert_eq!(report.bytes, size as u64);
    assert_eq!(report.lines, 1);
    assert_eq!(report.truncated_lines, 1);
    assert_eq!(report.read_error, None);
    assert!(!report.complete());
}

#[test]
fn complete_report_survives_many_prior_lines_without_retaining_history() {
    // One million short newline-terminated records, then the actual report.
    let reader = Repeated {
        left: 1024 * 1024,
        byte: b'\n',
    }
    .chain(Cursor::new(REPORT));
    let capture = capture_completion(reader);
    assert!(capture.report.complete());
    assert_eq!(capture.report.lines, 1024 * 1024 + 1);
    assert_eq!(capture.completion().unwrap().as_bytes(), REPORT);
    assert_eq!(capture.terminal.as_deref(), Some(REPORT));
}

#[test]
fn earlier_overflow_is_reported_but_terminal_document_is_retained() {
    let reader = Repeated {
        left: COMPLETION_LINE_LIMIT + 1,
        byte: b'x',
    }
    .chain(Cursor::new(b"\n"))
    .chain(Cursor::new(REPORT));
    let capture = capture_completion(reader);
    assert_eq!(capture.report.truncated_lines, 1);
    assert_eq!(capture.retained_document().unwrap().as_bytes(), REPORT);
    assert_eq!(capture.completion(), Err(CompletionError::Truncated));
}

#[test]
fn later_oversized_report_cannot_reuse_earlier_success() {
    let reader = Cursor::new(REPORT)
        .chain(Cursor::new(b"\n{"))
        .chain(Repeated {
            left: COMPLETION_LINE_LIMIT,
            byte: b' ',
        })
        .chain(Cursor::new(b"}\n"));
    let capture = capture_completion(reader);
    assert_eq!(capture.report.truncated_lines, 1);
    assert!(capture.retained_document().is_none());
    assert_eq!(capture.completion(), Err(CompletionError::Truncated));
}

#[test]
fn trailing_non_document_and_invalid_utf8_never_reuse_earlier_success() {
    let capture = capture_completion(
        Cursor::new(REPORT).chain(Cursor::new(b"\nviewer failed after reporting\n")),
    );
    assert_eq!(capture.completion(), Err(CompletionError::NotDocument));
    assert!(capture.retained_document().is_none());
    let invalid = capture_completion(Cursor::new(b"{\xff}\n"));
    assert_eq!(invalid.completion(), Err(CompletionError::InvalidUtf8));
    assert!(invalid.retained_document().is_none());
}

#[test]
fn empty_tail_and_crlf_preserve_the_original_complete_document() {
    let reader = Cursor::new(b"status\r\n  ")
        .chain(Cursor::new(REPORT))
        .chain(Cursor::new(b"\r\n\n \t\r\n"));
    let capture = capture_completion(reader);
    assert_eq!(capture.completion().unwrap().trim().as_bytes(), REPORT);
    assert!(capture.report.complete());
    assert_eq!(
        capture_completion(Cursor::new(b"\n \t\r\n")).completion(),
        Err(CompletionError::Missing)
    );
}

struct FaultReader {
    bytes: Cursor<Vec<u8>>,
    interrupted: bool,
}
impl Read for FaultReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if !self.interrupted {
            self.interrupted = true;
            return Err(io::ErrorKind::Interrupted.into());
        }
        match self.bytes.read(out)? {
            0 => Err(io::ErrorKind::BrokenPipe.into()),
            count => Ok(count),
        }
    }
}

#[test]
fn interruption_retries_but_late_pipe_error_prevents_success() {
    let mut bytes = REPORT.to_vec();
    bytes.push(b'\n');
    let capture = capture_completion(FaultReader {
        bytes: Cursor::new(bytes.clone()),
        interrupted: false,
    });
    assert_eq!(capture.retained_document().unwrap().as_bytes(), REPORT);
    assert_eq!(capture.report.bytes, bytes.len() as u64);
    assert_eq!(
        capture.completion(),
        Err(CompletionError::Read(io::ErrorKind::BrokenPipe))
    );
    assert!(!capture.report.complete());
}

#[test]
fn partial_line_at_pipe_error_is_not_a_completion() {
    let capture = capture_completion(FaultReader {
        bytes: Cursor::new(REPORT.to_vec()),
        interrupted: false,
    });
    assert!(capture.retained_document().is_none());
    assert_eq!(capture.report.lines, 0);
    assert_eq!(
        capture.completion(),
        Err(CompletionError::Read(io::ErrorKind::BrokenPipe))
    );
}

#[test]
fn line_budget_is_exact_and_independent_of_read_boundaries() {
    struct OneByte(Cursor<Vec<u8>>);
    impl Read for OneByte {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.0.read(&mut out[..1])
        }
    }
    let mut bytes = vec![b'a'; STATUS_LINE_LIMIT];
    bytes.extend_from_slice(b"\n");
    bytes.extend(std::iter::repeat_n(b'b', STATUS_LINE_LIMIT + 1));
    bytes.extend_from_slice(b"\nnext\n");
    let mut lines = Vec::new();
    let report = drain_status(OneByte(Cursor::new(bytes)), |bytes, truncated| {
        lines.push((bytes.to_vec(), truncated));
    });
    assert_eq!(report.lines, 3);
    assert_eq!(report.truncated_lines, 1);
    assert_eq!(lines[0], (vec![b'a'; STATUS_LINE_LIMIT], false));
    assert_eq!(lines[1], (vec![b'b'; STATUS_LINE_LIMIT], true));
    assert_eq!(lines[2], (b"next".to_vec(), false));
}

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
