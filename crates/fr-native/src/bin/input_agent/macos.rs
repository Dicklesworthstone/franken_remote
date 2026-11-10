//! The macOS role of `fr-input-agent` (fr-macos-host-control-b14s): the same
//! private protocol, launch contract and fail-closed rules as the Linux
//! executor, with Quartz events (`fr_native::macos_input`) as the native
//! owner and an `AppKit` panel as the control indicator.
//!
//! Launched by frd's `input_process` with a cleared environment,
//! `--parent-pid`, the command stream on stdin and the signal datagram socket
//! on stdout. It holds no lease, ticket, nonce, peer identity or network
//! socket: frd's `InputSession` makes every authority decision and sends a
//! `CLOCK_MONOTONIC` deadline. Before every event it re-checks that no fence
//! arrived, that the indicator was not used and that the deadline has not
//! passed; after a fence or local revoke only release-only cleanup, Cleanup
//! and Stop are accepted. EOF, Stop or a protocol violation releases what it
//! holds before exiting; a panic or the parent's exit releases the recorded
//! held set in the glue.
//!
//! `AppKit` owns the main thread: the indicator is shown and pumped there,
//! and the executor serves on a second thread. Only a local click on the
//! indicator (or hiding it) is a local revoke; a click this executor posted
//! is not. A first request `Probe` is frd's one-shot capability probe: no
//! event source, indicator or input ever exists in that process.
use crate::channel::{
    EXIT_CHANNEL, EXIT_PROTOCOL, EXIT_REFUSED, EXIT_USAGE, HELLO_TIMEOUT, command_channel,
    drain_signals, receive, reply, send, signal_channel,
};
use fr_core::input_submission::{
    Operation, PlatformError,
    process::{self, Reply, Request, Signal},
};
use fr_native::{
    clock,
    macos_input::{self, Indicator, IndicatorStatus, MacPointer},
    macos_input_state::Held,
};
use std::{
    os::unix::net::{UnixDatagram, UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

/// How long one indicator pump waits for events. Also bounds how long the
/// process outlives its executor thread.
const PUMP: Duration = Duration::from_millis(20);

pub fn run() -> i32 {
    let mut args = std::env::args().skip(1);
    let mut first = args.next();
    let clipboard = first.as_deref() == Some("--clipboard");
    if clipboard {
        first = args.next();
    }
    let parent = match (first.as_deref(), args.next(), args.next()) {
        (Some("--parent-pid"), Some(pid), None) => pid.parse::<u32>().ok(),
        _ => None,
    };
    // Refuse an already reparented launch before any native call.
    if parent.is_none_or(|pid| macos_input::bind_parent(pid).is_err()) {
        return EXIT_USAGE;
    }
    if clipboard {
        return command_channel().map_or(EXIT_USAGE, crate::clipboard::run);
    }
    let (Ok(mut command), Ok(signals)) = (command_channel(), signal_channel()) else {
        return EXIT_USAGE;
    };
    if command.set_read_timeout(Some(HELLO_TIMEOUT)).is_err() {
        return EXIT_CHANNEL;
    }
    let Ok(frame) = receive(&mut command) else {
        return EXIT_CHANNEL;
    };
    match process::decode_request(&frame) {
        Ok((
            1,
            Request::Hello {
                epoch,
                bounds,
                required,
            },
        )) => lease(command, signals, epoch, bounds, required),
        Ok((1, Request::Probe { epoch })) => probe(&mut command, epoch),
        _ => EXIT_PROTOCOL,
    }
}

/// The capabilities a lease executor would advertise, reported once.
fn probe(command: &mut UnixStream, epoch: u128) -> i32 {
    let reply = match macos_input::probe() {
        Ok(capabilities) => Reply::Ready {
            epoch,
            capabilities,
            repeat_requires_pair: false,
            line_scroll_requires_pairs: false,
        },
        Err(error) => Reply::Refused(error),
    };
    let refused = matches!(reply, Reply::Refused(_));
    match send(command, 1, reply) {
        Err(_) => EXIT_CHANNEL,
        Ok(()) if refused => EXIT_REFUSED,
        Ok(()) => 0,
    }
}

/// Open the owner and the indicator, reply Ready, then serve on a second
/// thread while this (main) thread keeps the indicator shown.
fn lease(
    mut command: UnixStream,
    signals: UnixDatagram,
    epoch: u128,
    bounds: fr_core::input::InputBounds,
    required: fr_core::input_submission::Capabilities,
) -> i32 {
    if command.set_read_timeout(None).is_err() {
        return EXIT_CHANNEL;
    }
    // The exact display binding and capabilities, the panic release path and
    // a shown indicator, all before any input can be accepted.
    let opened = MacPointer::open_exact(bounds, required).and_then(|pointer| {
        macos_input::install_panic_release();
        let indicator = Indicator::open(pointer.display_id())?;
        Ok((pointer, indicator))
    });
    let (pointer, indicator) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            let _ = send(&mut command, 1, Reply::Refused(error));
            return EXIT_REFUSED;
        }
    };
    let Ok(notify) = signals.try_clone() else {
        let _ = send(&mut command, 1, Reply::Refused(PlatformError::Unavailable));
        return EXIT_REFUSED;
    };
    let ready = Reply::Ready {
        epoch,
        capabilities: pointer.capabilities(),
        repeat_requires_pair: pointer.repeat_requires_pair(),
        line_scroll_requires_pairs: pointer.line_scroll_requires_pairs(),
    };
    let revoked = Arc::new(AtomicBool::new(false));
    let mut executor = Executor {
        pointer: Some(pointer),
        revoked: revoked.clone(),
        command,
        signals,
        expected: 2,
        prepared: None,
        fenced: false,
    };
    executor.drain_signals();
    if indicator.pump(Duration::ZERO) != IndicatorStatus::Shown || executor.revoked() {
        let _ = send(
            &mut executor.command,
            1,
            Reply::Refused(PlatformError::Permission),
        );
        return EXIT_REFUSED;
    }
    if send(&mut executor.command, 1, ready).is_err() {
        return EXIT_CHANNEL;
    }
    let Ok(worker) = thread::Builder::new()
        .name("fr-input-executor".into())
        .spawn(move || executor.serve())
    else {
        return EXIT_CHANNEL;
    };
    while !worker.is_finished() {
        let status = indicator.pump(PUMP);
        // Its first stop sends exactly one datagram: `LocalRevoke` when the
        // local user caused it, otherwise `IndicatorFailed`.
        if status != IndicatorStatus::Shown && !revoked.swap(true, Ordering::AcqRel) {
            let signal = if status == IndicatorStatus::LocalStop {
                Signal::LocalRevoke
            } else {
                Signal::IndicatorFailed
            };
            let _ = notify.send(&process::encode_signal(signal));
        }
    }
    // A panic on the executor thread already released and exited (the hook).
    let code = worker.join().unwrap_or(macos_input::EXIT_PANIC);
    drop(indicator);
    code
}

struct Executor {
    pointer: Option<MacPointer>,
    revoked: Arc<AtomicBool>,
    command: UnixStream,
    signals: UnixDatagram,
    expected: u64,
    prepared: Option<Operation>,
    fenced: bool,
}

impl Executor {
    fn serve(mut self) -> i32 {
        loop {
            // EOF (frd gone or closed) ends here; Drop releases.
            let Ok(frame) = receive(&mut self.command) else {
                return EXIT_CHANNEL;
            };
            let Ok((sequence, request)) = process::decode_request(&frame) else {
                return EXIT_PROTOCOL;
            };
            if sequence != self.expected {
                return EXIT_PROTOCOL;
            }
            let Some(next) = sequence.checked_add(1) else {
                return EXIT_PROTOCOL;
            };
            self.expected = next;
            self.drain_signals();
            let reply = match request {
                Request::Hello { .. } | Request::Probe { .. } => return EXIT_PROTOCOL,
                Request::Prepare(operation) => self.prepare(operation),
                Request::Submit {
                    operation,
                    not_after_ns,
                } => self.submit(operation, not_after_ns),
                Request::Release(operation) => self.release(operation),
                Request::Cancel => {
                    self.cancel();
                    Reply::Cancelled
                }
                Request::Cleanup => {
                    self.prepared = None;
                    Reply::Cleaned(self.pointer().cleanup_native())
                }
                Request::Stop => {
                    // Release BEFORE acknowledging: the broker may kill the
                    // group once Stopped arrives.
                    self.prepared = None;
                    drop(self.pointer.take());
                    macos_input::record(Held::default());
                    let _ = send(&mut self.command, sequence, Reply::Stopped);
                    return 0;
                }
            };
            macos_input::record(self.pointer().native_held());
            if send(&mut self.command, sequence, reply).is_err() {
                return EXIT_CHANNEL;
            }
        }
    }
    fn pointer(&mut self) -> &mut MacPointer {
        self.pointer
            .as_mut()
            .expect("the Quartz owner lives until Stop")
    }
    fn cancel(&mut self) {
        self.prepared = None;
        self.pointer().cancel_prepared();
    }
    fn drain_signals(&mut self) {
        drain_signals(&self.signals, &mut self.fenced);
    }
    fn revoked(&self) -> bool {
        self.fenced || self.revoked.load(Ordering::Acquire)
    }
    fn prepare(&mut self, operation: Operation) -> Reply {
        self.cancel();
        if self.revoked() && !operation.is_release() {
            return Reply::Fenced;
        }
        match self.pointer().prepare(operation) {
            Ok(()) => {
                self.prepared = Some(operation);
                Reply::Prepared
            }
            Err(error) => Reply::PrepareFailed(error),
        }
    }
    fn submit(&mut self, operation: Operation, not_after_ns: u64) -> Reply {
        if self.prepared.take() != Some(operation) {
            self.pointer().cancel_prepared();
            return Reply::NotSubmitted(PlatformError::Unsupported);
        }
        let Self {
            pointer,
            signals,
            fenced,
            revoked,
            ..
        } = self;
        let pointer = pointer.as_mut().expect("the Quartz owner lives until Stop");
        // The display binding is revalidated again at Submit, since the
        // Prepared reply crossed an IPC boundary. Only THEN sample the
        // deadline and the independently received fence.
        match pointer.submit_checked(operation, |held| {
            macos_input::record(held);
            drain_signals(signals, fenced);
            if *fenced || revoked.load(Ordering::Acquire) {
                return Err(Reply::Fenced);
            }
            if clock::monotonic_ns().is_none_or(|now| now >= not_after_ns) {
                return Err(Reply::Expired);
            }
            Ok(())
        }) {
            Ok(submission) => reply(submission),
            Err(refusal) => refusal,
        }
    }
    /// Release-only cleanup: no deadline, accepted after a fence too.
    fn release(&mut self, operation: Operation) -> Reply {
        if !operation.is_release() || self.prepared.take() != Some(operation) {
            self.pointer().cancel_prepared();
            return Reply::NotSubmitted(PlatformError::Unsupported);
        }
        reply(self.pointer().submit(operation))
    }
}
