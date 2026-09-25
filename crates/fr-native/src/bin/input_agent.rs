#![forbid(unsafe_code)]
//! `fr-input-agent`: the per-lease out-of-process input injection executor.
//!
//! Launched only by frd's `input_process` factory with a cleared environment
//! (DISPLAY, optional XAUTHORITY), `--parent-pid`, a private `SOCK_STREAM`
//! command channel on stdin and a private `SOCK_DGRAM` signal channel on
//! stdout. It owns the X11 connection for exactly one lease and holds no lease,
//! ticket, nonce, peer identity or network socket: frd's canonical
//! `InputSession` performs every authority check and sends only a derived
//! `CLOCK_MONOTONIC` deadline.
//!
//! Immediately before every native call it re-checks that no fence arrived and
//! that its own clock is strictly before that deadline; otherwise it reports
//! Fenced/Expired and makes no call. After a fence only release-only cleanup,
//! Cleanup and Stop are accepted. On EOF, Stop or a protocol violation it
//! releases and restores through `X11Pointer`'s own teardown before exiting; a
//! panic or Xlib error releases through the dedicated emergency connection.
//! It writes nothing but reply frames and signal datagrams, and logs no input.
#[cfg(target_os = "linux")]
mod linux {
    use fr_core::input_submission::{
        InputSink, Operation, PlatformError, Submission,
        process::{self, Reply, Request},
    };
    use fr_native::{
        bind_parent, clock,
        input::{NativeHeld, X11Pointer, emergency},
    };
    use std::{
        io::{self, Read, Write},
        os::{
            fd::AsFd,
            unix::net::{UnixDatagram, UnixStream},
        },
        time::Duration,
    };

    const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
    const SIGNALS_PER_CHECK: usize = 8;
    const EXIT_USAGE: i32 = 64;
    const EXIT_PROTOCOL: i32 = 65;
    const EXIT_REFUSED: i32 = 66;
    const EXIT_CHANNEL: i32 = 67;

    pub fn run() -> i32 {
        let mut args = std::env::args().skip(1);
        let parent = match (args.next().as_deref(), args.next(), args.next()) {
            (Some("--parent-pid"), Some(pid), None) => pid.parse::<u32>().ok(),
            _ => None,
        };
        // Refuse an already reparented launch before touching X11.
        if parent.is_none_or(|pid| bind_parent(pid).is_err()) {
            return EXIT_USAGE;
        }
        let (Ok(command), Ok(signals)) = (command_channel(), signal_channel()) else {
            return EXIT_USAGE;
        };
        match Executor::open(command, signals) {
            Ok(executor) => executor.serve(),
            Err(code) => code,
        }
    }
    fn command_channel() -> io::Result<UnixStream> {
        let stream = UnixStream::from(io::stdin().as_fd().try_clone_to_owned()?);
        // Only a connected private socket is accepted, never a file or tty.
        stream.peer_addr()?;
        Ok(stream)
    }
    fn signal_channel() -> io::Result<UnixDatagram> {
        let socket = UnixDatagram::from(io::stdout().as_fd().try_clone_to_owned()?);
        socket.peer_addr()?;
        socket.set_nonblocking(true)?;
        Ok(socket)
    }
    fn send(command: &mut UnixStream, sequence: u64, reply: Reply) -> io::Result<()> {
        let frame = process::encode_reply(sequence, reply)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        command.write_all(&frame)
    }
    fn receive(command: &mut UnixStream) -> io::Result<[u8; process::FRAME_BYTES]> {
        let mut frame = [0; process::FRAME_BYTES];
        command.read_exact(&mut frame)?;
        Ok(frame)
    }
    fn reply(submission: Submission) -> Reply {
        match submission {
            Submission::Submitted => Reply::Submitted,
            Submission::NotSubmitted(error) => Reply::NotSubmitted(error),
            Submission::Unknown => Reply::Unknown,
            Submission::Expired => Reply::Expired,
            Submission::Fenced => Reply::Fenced,
        }
    }

    struct Executor {
        pointer: Option<X11Pointer>,
        command: UnixStream,
        signals: UnixDatagram,
        expected: u64,
        prepared: Option<Operation>,
        fenced: bool,
    }
    impl Executor {
        fn open(mut command: UnixStream, signals: UnixDatagram) -> Result<Self, i32> {
            command
                .set_read_timeout(Some(HELLO_TIMEOUT))
                .map_err(|_| EXIT_CHANNEL)?;
            let frame = receive(&mut command).map_err(|_| EXIT_CHANNEL)?;
            let Ok((
                1,
                Request::Hello {
                    epoch,
                    bounds,
                    required,
                },
            )) = process::decode_request(&frame)
            else {
                return Err(EXIT_PROTOCOL);
            };
            command.set_read_timeout(None).map_err(|_| EXIT_CHANNEL)?;
            let opened = std::env::var("DISPLAY")
                .map_err(|_| PlatformError::Unsupported)
                .and_then(|display| {
                    // Same exact bounds/capability revalidation as the
                    // in-process factory, then the emergency release path,
                    // both before any input can be accepted.
                    let pointer = X11Pointer::open_exact(&display, bounds, required)?;
                    emergency::install(&display)?;
                    Ok(pointer)
                });
            let pointer = match opened {
                Ok(pointer) => pointer,
                Err(error) => {
                    let _ = send(&mut command, 1, Reply::Refused(error));
                    return Err(EXIT_REFUSED);
                }
            };
            let ready = Reply::Ready {
                epoch,
                capabilities: pointer.capabilities(),
                repeat_requires_pair: pointer.repeat_requires_pair(),
                line_scroll_requires_pairs: pointer.line_scroll_requires_pairs(),
            };
            let mut executor = Self {
                pointer: Some(pointer),
                command,
                signals,
                expected: 2,
                prepared: None,
                fenced: false,
            };
            executor.drain_signals();
            if executor.fenced {
                let _ = send(
                    &mut executor.command,
                    1,
                    Reply::Refused(PlatformError::Permission),
                );
                return Err(EXIT_REFUSED);
            }
            send(&mut executor.command, 1, ready).map_err(|_| EXIT_CHANNEL)?;
            Ok(executor)
        }
        fn serve(mut self) -> i32 {
            loop {
                // EOF (frd gone or closed) ends here; Drop releases/restores.
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
                    Request::Hello { .. } => return EXIT_PROTOCOL,
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
                        // Release, restore and close X11 BEFORE acknowledging:
                        // the broker may kill the group once Stopped arrives.
                        self.prepared = None;
                        drop(self.pointer.take());
                        emergency::record(NativeHeld::default());
                        let _ = send(&mut self.command, sequence, Reply::Stopped);
                        return 0;
                    }
                };
                emergency::record(self.pointer().native_held());
                if send(&mut self.command, sequence, reply).is_err() {
                    return EXIT_CHANNEL;
                }
            }
        }
        fn pointer(&mut self) -> &mut X11Pointer {
            self.pointer
                .as_mut()
                .expect("the X11 owner lives until Stop")
        }
        fn cancel(&mut self) {
            self.prepared = None;
            self.pointer().cancel_prepared();
        }
        /// A Fence datagram fences. So does anything else on the private
        /// channel (malformed, wrong direction) or a broken channel: the
        /// executor fails closed and never reopens.
        fn drain_signals(&mut self) {
            let mut buffer = [0; process::SIGNAL_BYTES + 1];
            for _ in 0..SIGNALS_PER_CHECK {
                match self.signals.recv(&mut buffer) {
                    Ok(_) => self.fenced = true,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        self.fenced = true;
                        return;
                    }
                }
            }
        }
        fn revoked(&self) -> bool {
            self.fenced
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
            // Final executor checks, immediately before the native call.
            self.drain_signals();
            if self.revoked() {
                self.pointer().cancel_prepared();
                return Reply::Fenced;
            }
            if clock::monotonic_ns().is_none_or(|now| now >= not_after_ns) {
                self.pointer().cancel_prepared();
                return Reply::Expired;
            }
            let pointer = self.pointer();
            emergency::record(pointer.native_held());
            reply(pointer.submit(operation))
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
}

fn main() {
    #[cfg(target_os = "linux")]
    std::process::exit(linux::run());
    #[cfg(not(target_os = "linux"))]
    std::process::exit(2);
}
