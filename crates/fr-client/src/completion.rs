//! Bounded drainage of a native viewer's completion and status streams.
//!
//! The final nonempty line owns the completion: a later invalid line never
//! falls back to an older document. Readers drain the entire stream while
//! retaining at most one bounded line plus one bounded terminal candidate.
//! Parsing the document and checking the original process remain the caller's
//! responsibility. These bytes must be content-free viewer diagnostics.
#![forbid(unsafe_code)]

use std::io::{self, Read};

/// Completion documents include bounded file receipts and cadence summaries.
pub const COMPLETION_LINE_LIMIT: usize = 256 * 1024;
/// The native CLI producer permits completion documents up to two MiB.
pub const NATIVE_COMPLETION_LINE_LIMIT: usize = 2 * 1024 * 1024;
/// Typed status lines normally occupy only a few hundred bytes.
pub const STATUS_LINE_LIMIT: usize = 4096;
const READ_CHUNK_BYTES: usize = 8192;

/// Counters saturate instead of overflowing during an arbitrarily long run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrainReport {
    pub bytes: u64,
    pub lines: u64,
    pub truncated_lines: u64,
    pub read_error: Option<io::ErrorKind>,
}

impl DrainReport {
    pub fn complete(self) -> bool {
        self.truncated_lines == 0 && self.read_error.is_none()
    }
}

/// Drain stderr through a fixed-size line owner. An oversized line is reported
/// once with its retained prefix and `truncated = true`; its remaining bytes
/// are discarded through the next newline before another line is admitted.
/// Callback/log failures must not interrupt draining the child's pipe.
pub fn drain_status(reader: impl Read, consume: impl FnMut(&[u8], bool)) -> DrainReport {
    drain_lines(reader, STATUS_LINE_LIMIT, consume)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionError {
    Read(io::ErrorKind),
    Truncated,
    Missing,
    InvalidUtf8,
    NotDocument,
}

/// The two existing output formats have different admitted document budgets.
/// A closed choice keeps callers from requesting an unbounded allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionLimit {
    WindowsViewer,
    NativeCommand,
}

impl CompletionLimit {
    const fn bytes(self) -> usize {
        match self {
            Self::WindowsViewer => COMPLETION_LINE_LIMIT,
            Self::NativeCommand => NATIVE_COMPLETION_LINE_LIMIT,
        }
    }
}

/// Only the final nonempty line is a possible completion. A later malformed
/// or oversized line invalidates an older plausible document rather than
/// accidentally reporting an earlier successful session.
#[derive(Debug)]
pub struct CompletionCapture {
    pub report: DrainReport,
    terminal: Option<Vec<u8>>,
    terminal_truncated: bool,
}

impl CompletionCapture {
    /// Retain a complete terminal document for diagnostics even when an earlier
    /// line or later pipe read failed. Callers must use `completion` to decide
    /// whether it can support a successful session notice.
    pub fn retained_document(&self) -> Option<&str> {
        if self.terminal_truncated {
            return None;
        }
        let line = std::str::from_utf8(self.terminal.as_deref()?).ok()?;
        line.trim_start().starts_with('{').then_some(line)
    }

    pub fn completion(&self) -> Result<&str, CompletionError> {
        if let Some(error) = self.report.read_error {
            return Err(CompletionError::Read(error));
        }
        if self.report.truncated_lines != 0 {
            return Err(CompletionError::Truncated);
        }
        let bytes = self.terminal.as_deref().ok_or(CompletionError::Missing)?;
        let line = std::str::from_utf8(bytes).map_err(|_| CompletionError::InvalidUtf8)?;
        if !line.trim_start().starts_with('{') {
            return Err(CompletionError::NotDocument);
        }
        Ok(line)
    }
}

pub fn capture_completion(reader: impl Read) -> CompletionCapture {
    capture_completion_with_limit(reader, CompletionLimit::WindowsViewer)
}

pub fn capture_completion_with_limit(
    reader: impl Read,
    limit: CompletionLimit,
) -> CompletionCapture {
    let mut terminal = None;
    let mut terminal_truncated = false;
    let report = drain_lines(reader, limit.bytes(), |line, truncated| {
        if truncated || !line.iter().all(u8::is_ascii_whitespace) {
            terminal = Some(line.to_vec());
            terminal_truncated = truncated;
        }
    });
    CompletionCapture {
        report,
        terminal,
        terminal_truncated,
    }
}

fn drain_lines(
    mut reader: impl Read,
    limit: usize,
    mut consume: impl FnMut(&[u8], bool),
) -> DrainReport {
    let mut report = DrainReport::default();
    let mut line = Vec::with_capacity(limit);
    let mut truncated = false;
    let mut chunk = [0u8; READ_CHUNK_BYTES];
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                report.read_error = Some(error.kind());
                // An incomplete line at a failed read cannot become a report.
                return report;
            }
        };
        report.bytes = report.bytes.saturating_add(count as u64);
        let mut pending = &chunk[..count];
        while !pending.is_empty() {
            let end = pending.iter().position(|&byte| byte == b'\n');
            let part = &pending[..end.unwrap_or(pending.len())];
            let retain = part.len().min(limit - line.len());
            line.extend_from_slice(&part[..retain]);
            truncated |= retain != part.len();
            let Some(end) = end else { break };
            emit_line(&mut report, &line, truncated, &mut consume);
            line.clear();
            truncated = false;
            pending = &pending[end + 1..];
        }
    }
    if !line.is_empty() || truncated {
        emit_line(&mut report, &line, truncated, &mut consume);
    }
    report
}

fn emit_line(
    report: &mut DrainReport,
    bytes: &[u8],
    truncated: bool,
    consume: &mut impl FnMut(&[u8], bool),
) {
    report.lines = report.lines.saturating_add(1);
    if truncated {
        report.truncated_lines = report.truncated_lines.saturating_add(1);
    }
    // Strip CR only from an intact line; a retained prefix is not a CRLF end.
    let bytes = if truncated {
        bytes
    } else {
        bytes.strip_suffix(b"\r").unwrap_or(bytes)
    };
    consume(bytes, truncated);
}

#[cfg(test)]
#[path = "completion/tests.rs"]
mod tests;
