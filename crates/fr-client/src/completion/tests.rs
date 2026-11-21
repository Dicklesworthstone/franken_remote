use super::*;
use std::io::Cursor;

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

#[test]
fn native_command_documents_keep_their_existing_two_mib_budget() {
    let mut document = vec![b' '; NATIVE_COMPLETION_LINE_LIMIT];
    document[0] = b'{';
    document[NATIVE_COMPLETION_LINE_LIMIT - 1] = b'}';
    let capture =
        capture_completion_with_limit(document.as_slice(), CompletionLimit::NativeCommand);
    assert_eq!(
        capture.completion().unwrap().len(),
        NATIVE_COMPLETION_LINE_LIMIT
    );
    assert!(capture.report.complete());
    // Windows retains its own smaller established budget.
    assert_eq!(
        capture_completion(document.as_slice()).completion(),
        Err(CompletionError::Truncated)
    );
    document.push(b' ');
    let oversized =
        capture_completion_with_limit(document.as_slice(), CompletionLimit::NativeCommand);
    assert_eq!(oversized.completion(), Err(CompletionError::Truncated));
    assert!(oversized.retained_document().is_none());
}
