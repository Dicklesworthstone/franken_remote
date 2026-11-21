//! Original child ownership while both bounded pipe readers run concurrently.
//!
//! The two completion slots contain at most one bounded result per reader.
//! A read error, reader panic or thread-launch failure stops and reaps the
//! original viewer. Captured reports and cleanup failures remain separate;
//! neither an exit code nor a previously printed JSON object proves success.
#![forbid(unsafe_code)]

use super::{CappedLog, CompletionCapture, DrainReport, capture_completion, drain_status};
use std::{
    io::{self, Read},
    panic::{AssertUnwindSafe, catch_unwind},
    process::{Child, ExitStatus},
    sync::mpsc::{self, SyncSender},
    thread::{self, JoinHandle},
    time::Instant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FollowError {
    MissingPipe,
    Spawn(Stream, io::ErrorKind),
    Read(Stream, io::ErrorKind),
    ReaderPanicked(Stream),
    ReaderDisconnected,
    Wait(io::ErrorKind),
}

/// The first line is only bounded refusal guidance, never a success document.
pub(crate) struct StatusCapture {
    pub report: DrainReport,
    pub first_line: Option<Vec<u8>>,
    pub log_error: Option<io::ErrorKind>,
}

#[derive(Default)]
pub(crate) struct ChildOutput {
    pub status: Option<ExitStatus>,
    pub stdout: Option<CompletionCapture>,
    pub stderr: Option<StatusCapture>,
    /// The same log handle returns after the stderr writer has joined.
    pub log: Option<CappedLog>,
    pub failure: Option<FollowError>,
    pub cleanup_error: Option<io::ErrorKind>,
}

impl ChildOutput {
    /// Necessary, not sufficient: the UI must also evaluate the completion's
    /// session/transfer result. Initial log-open errors remain caller-owned.
    pub fn complete(&self) -> bool {
        self.failure.is_none()
            && self.cleanup_error.is_none()
            && self.status.is_some_and(|status| status.success())
            && self
                .stdout
                .as_ref()
                .is_some_and(|capture| capture.completion().is_ok())
            && self
                .stderr
                .as_ref()
                .is_some_and(|capture| capture.report.complete() && capture.log_error.is_none())
    }

    fn observe(&mut self, event: Event) {
        match event {
            Event::Stdout(capture) => {
                if let Some(error) = capture.report.read_error {
                    self.failure
                        .get_or_insert(FollowError::Read(Stream::Stdout, error));
                }
                self.stdout = Some(capture);
            }
            Event::Stderr(capture, log) => {
                if let Some(error) = capture.report.read_error {
                    self.failure
                        .get_or_insert(FollowError::Read(Stream::Stderr, error));
                }
                self.stderr = Some(capture);
                self.log = log;
            }
            Event::Panicked(stream) => {
                self.failure
                    .get_or_insert(FollowError::ReaderPanicked(stream));
            }
        }
    }
}

enum Event {
    Stdout(CompletionCapture),
    Stderr(StatusCapture, Option<CappedLog>),
    Panicked(Stream),
}

/// Follow a viewer whose stdout and stderr were piped at spawn. No command
/// arguments, machine identifiers or paths enter the log through this API.
/// Pipe readers are joined before the log returns to its caller.
pub(crate) fn follow_child(mut child: Child, log: Option<CappedLog>) -> ChildOutput {
    // This follower never sends commands to the viewer.
    drop(child.stdin.take());
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let mut output = ChildOutput {
            log,
            failure: Some(FollowError::MissingPipe),
            ..ChildOutput::default()
        };
        terminate(&mut child, &mut output);
        return output;
    };
    follow_pipes(child, stdout, stderr, log)
}

fn follow_pipes(
    mut child: Child,
    stdout: impl Read + Send + 'static,
    stderr: impl Read + Send + 'static,
    log: Option<CappedLog>,
) -> ChildOutput {
    let mut output = ChildOutput {
        log,
        ..ChildOutput::default()
    };
    let (sender, receiver) = mpsc::sync_channel(2);
    let stdout_reader = match spawn_reader(Stream::Stdout, sender.clone(), move || {
        Event::Stdout(capture_completion(stdout))
    }) {
        Ok(reader) => reader,
        Err(error) => {
            output.failure = Some(FollowError::Spawn(Stream::Stdout, error.kind()));
            // Drop the unstarted pipe before waiting for the original process.
            drop(stderr);
            terminate(&mut child, &mut output);
            return output;
        }
    };
    let log = output.log.take();
    let stderr_reader = match spawn_reader(Stream::Stderr, sender.clone(), move || {
        let (capture, log) = capture_status(stderr, log);
        Event::Stderr(capture, log)
    }) {
        Ok(reader) => reader,
        Err(error) => {
            output.failure = Some(FollowError::Spawn(Stream::Stderr, error.kind()));
            terminate(&mut child, &mut output);
            join_reader(stdout_reader, Stream::Stdout, &mut child, &mut output);
            // Only the started reader can have produced a bounded result.
            if let Ok(event) = receiver.try_recv() {
                output.observe(event);
            }
            return output;
        }
    };
    drop(sender);
    let mut terminated = false;
    for _ in 0..2 {
        match receiver.recv() {
            Ok(event) => output.observe(event),
            Err(_) => {
                output
                    .failure
                    .get_or_insert(FollowError::ReaderDisconnected);
            }
        }
        if output.failure.is_some() && !terminated {
            terminated = true;
            terminate(&mut child, &mut output);
        }
    }
    join_reader(stdout_reader, Stream::Stdout, &mut child, &mut output);
    join_reader(stderr_reader, Stream::Stderr, &mut child, &mut output);
    reap(&mut child, &mut output);
    output
}

fn spawn_reader(
    stream: Stream,
    sender: SyncSender<Event>,
    work: impl FnOnce() -> Event + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    let name = match stream {
        Stream::Stdout => "fr-session-report",
        Stream::Stderr => "fr-session-status",
    };
    thread::Builder::new().name(name.into()).spawn(move || {
        let event = match catch_unwind(AssertUnwindSafe(work)) {
            Ok(event) => event,
            Err(_) => Event::Panicked(stream),
        };
        // Two producers send at most once each into exactly two slots.
        // A vanished receiver means its owner is already ending.
        let _ = sender.send(event);
    })
}

fn capture_status(
    stderr: impl Read,
    mut log: Option<CappedLog>,
) -> (StatusCapture, Option<CappedLog>) {
    let opened = Instant::now();
    let mut first_line = None;
    let mut log_error = None;
    let report = drain_status(stderr, |line, truncated| {
        if first_line.is_none() && !line.iter().all(u8::is_ascii_whitespace) {
            first_line = Some(line.to_vec());
        }
        let error = log.as_mut().and_then(|log| {
            let mut stamped = format!("{:8.1}s ", opened.elapsed().as_secs_f64()).into_bytes();
            stamped.extend_from_slice(line);
            if truncated {
                stamped.extend_from_slice(b" [status line truncated]");
            }
            stamped.push(b'\n');
            log.append(&stamped).err().map(|error| error.kind())
        });
        if let Some(error) = error {
            log_error.get_or_insert(error);
            // A failed/contended log cannot block or repeatedly retry ahead
            // of pipe draining. Report it after the original viewer ends.
            log = None;
        }
    });
    (
        StatusCapture {
            report,
            first_line,
            log_error,
        },
        log,
    )
}

fn join_reader(
    reader: JoinHandle<()>,
    stream: Stream,
    child: &mut Child,
    output: &mut ChildOutput,
) {
    if reader.join().is_err() {
        output
            .failure
            .get_or_insert(FollowError::ReaderPanicked(stream));
        terminate(child, output);
    }
}

fn terminate(child: &mut Child, output: &mut ChildOutput) {
    if output.status.is_some() {
        return;
    }
    match child.try_wait() {
        Ok(Some(status)) => {
            output.status = Some(status);
            return;
        }
        Ok(None) => {}
        Err(error) => {
            output.cleanup_error.get_or_insert(error.kind());
        }
    }
    if let Err(error) = child.kill() {
        // A child can exit between try_wait and kill. Only the observed exit
        // settles that race; every other kill error remains explicit.
        match child.try_wait() {
            Ok(Some(status)) => output.status = Some(status),
            _ => {
                output.cleanup_error.get_or_insert(error.kind());
            }
        }
    }
    reap(child, output);
}

fn reap(child: &mut Child, output: &mut ChildOutput) {
    if output.status.is_some() {
        return;
    }
    loop {
        match child.wait() {
            Ok(status) => {
                output.status = Some(status);
                return;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                output
                    .failure
                    .get_or_insert(FollowError::Wait(error.kind()));
                output.cleanup_error.get_or_insert(error.kind());
                return;
            }
        }
    }
}

#[cfg(test)]
#[path = "child/tests.rs"]
mod tests;
