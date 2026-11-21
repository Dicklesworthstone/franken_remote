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
#[cfg(test)]
pub(crate) use fr_client::completion::COMPLETION_LINE_LIMIT;
pub(crate) use fr_client::completion::{
    CompletionCapture, CompletionError, DrainReport, STATUS_LINE_LIMIT, capture_completion,
    drain_status,
};

const OMITTED_LOG: &[u8] = b"[older session log omitted]\n";
const OMITTED_ENTRY: &[u8] = b"[oversized session log entry truncated]\n";

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
