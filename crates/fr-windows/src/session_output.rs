//! Bounded custody of a desktop viewer's output and local session log.
//!
//! A reconnecting viewer may run indefinitely. Pipe readers therefore drain
//! every byte while retaining only bounded lines, and a log append compacts
//! before it can cross its byte ceiling. Dropped output and read/write failures
//! remain explicit; a truncated or failed capture is never a successful report.
//! This module handles bytes, not command arguments: callers must supply only
//! the viewer's content-free diagnostics, never selected local paths.
#![forbid(unsafe_code)]

use std::{
    fs::{File, OpenOptions, TryLockError},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

#[path = "session_output/child.rs"]
mod child;
#[cfg(windows)]
pub(crate) use child::{ChildOutput, follow_child};
#[path = "session_output/report.rs"]
mod report;
#[cfg(windows)]
pub(crate) use report::summarize;

/// Includes all prior sessions and the currently running session.
pub(crate) const LOG_LIMIT: usize = 1024 * 1024;
/// Completion documents include bounded file receipts and cadence summaries.
pub(crate) const COMPLETION_LINE_LIMIT: usize = 256 * 1024;
/// Typed status lines normally occupy only a few hundred bytes.
pub(crate) const STATUS_LINE_LIMIT: usize = 4096;
const READ_CHUNK_BYTES: usize = 8192;
const OMITTED_LOG: &[u8] = b"[older session log omitted]\n";
const OMITTED_ENTRY: &[u8] = b"[oversized session log entry truncated]\n";

/// Counters saturate instead of overflowing during an arbitrarily long run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DrainReport {
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
pub(crate) fn drain_status(reader: impl Read, consume: impl FnMut(&[u8], bool)) -> DrainReport {
    drain_lines(reader, STATUS_LINE_LIMIT, consume)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionError {
    Read(io::ErrorKind),
    Truncated,
    Missing,
    InvalidUtf8,
    NotDocument,
}

/// Only the final nonempty line is a possible completion. A later malformed
/// or oversized line invalidates an older plausible document rather than
/// accidentally reporting an earlier successful session.
pub(crate) struct CompletionCapture {
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

pub(crate) fn capture_completion(reader: impl Read) -> CompletionCapture {
    let mut terminal = None;
    let mut terminal_truncated = false;
    let report = drain_lines(reader, COMPLETION_LINE_LIMIT, |line, truncated| {
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

/// One handle to the capped local log. A nonblocking OS file lock serializes
/// compaction with other app instances. Contention or disk failure is returned
/// to the caller; logging must never hold up pipe draining or viewer cleanup.
pub(crate) struct CappedLog {
    file: File,
}

impl CappedLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let mut log = Self { file };
        // A previous version could leave an enormous log. This reads at most
        // half the cap from its suffix, never the entire existing file.
        log.append(&[])?;
        Ok(log)
    }

    pub fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
            TryLockError::Error(error) => error,
        })?;
        let result = append_locked(&mut self.file, bytes);
        let unlock = self.file.unlock();
        result.and(unlock)
    }
}

fn append_locked(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    let length = file.metadata()?.len();
    let fits = length
        .checked_add(bytes.len() as u64)
        .is_some_and(|total| total <= LOG_LIMIT as u64);
    if fits {
        file.seek(SeekFrom::End(0))?;
        return file.write_all(bytes);
    }

    // Even an unexpectedly large caller-owned entry cannot grow the file past
    // the cap. Keep its bounded prefix with an explicit omission marker.
    if bytes.len() > LOG_LIMIT - OMITTED_ENTRY.len() {
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        file.write_all(OMITTED_ENTRY)?;
        return file.write_all(&bytes[..LOG_LIMIT - OMITTED_ENTRY.len()]);
    }

    let keep = (LOG_LIMIT - OMITTED_LOG.len() - bytes.len()).min(LOG_LIMIT / 2);
    let keep = u64::try_from(keep).unwrap_or(0).min(length);
    file.seek(SeekFrom::Start(length - keep))?;
    let mut tail = vec![0; usize::try_from(keep).unwrap_or(0)];
    file.read_exact(&mut tail)?;
    // The old suffix might begin inside a line. Drop that partial line so a
    // retained JSON suffix or status fragment cannot masquerade as an event.
    let start = if keep == length {
        0
    } else {
        tail.iter()
            .position(|&byte| byte == b'\n')
            .map_or(tail.len(), |end| end + 1)
    };
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(OMITTED_LOG)?;
    file.write_all(&tail[start..])?;
    file.write_all(bytes)
}

#[cfg(test)]
#[path = "session_output/tests.rs"]
mod tests;
