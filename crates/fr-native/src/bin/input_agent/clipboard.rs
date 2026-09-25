//! `fr-input-agent --clipboard`: the per-lease out-of-process X11 CLIPBOARD
//! owner for frd's controlled share (frd never loads Xlib/XCB).
//!
//! Launched only by frd's `clipboard_process` factory, on the clipboard worker
//! thread, after the controller's input grant and bilateral clipboard
//! readiness, with a cleared environment (DISPLAY, optional XAUTHORITY),
//! `--parent-pid` and a private `SOCK_STREAM` command channel on stdin. It
//! holds no lease, ticket, peer identity or network socket: frd's synchronizer
//! performs every authority check. This child only executes one bounded native
//! request at a time, and re-checks frd's publication deadline in its own
//! `CLOCK_MONOTONIC` immediately before the X11 ownership call.
//!
//! Payloads are bounded by the item size agreed in `Hello`, checked from the
//! frame before any allocation. On EOF, Stop or a protocol violation it erases
//! its private copies and closes X11 without resetting another application's
//! newer selection. It writes nothing but reply frames and never logs text.
use crate::linux::{EXIT_CHANNEL, EXIT_PROTOCOL, EXIT_REFUSED};
use fr_core::clipboard::process::{self, Reply, Request};
use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    time::Duration,
};

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// frd reads every reply at once; a reader that stops is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

fn receive(command: &mut UnixStream) -> io::Result<[u8; process::FRAME_BYTES]> {
    let mut frame = [0; process::FRAME_BYTES];
    command.read_exact(&mut frame)?;
    Ok(frame)
}
fn send(command: &mut UnixStream, sequence: u64, reply: Reply, payload: &[u8]) -> io::Result<()> {
    let frame = process::encode_reply(sequence, reply)
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    command.write_all(&frame)?;
    command.write_all(payload)
}
/// Read the Hello: the launch epoch and the agreed item bound, or an exit code.
fn hello(command: &mut UnixStream) -> Result<(u128, u32), i32> {
    command
        .set_read_timeout(Some(HELLO_TIMEOUT))
        .map_err(|_| EXIT_CHANNEL)?;
    let frame = receive(command).map_err(|_| EXIT_CHANNEL)?;
    let Ok((
        1,
        Request::Hello {
            epoch,
            max_item_bytes,
        },
    )) = process::decode_request(&frame, process::MAX_ITEM_BYTES)
    else {
        return Err(EXIT_PROTOCOL);
    };
    command.set_read_timeout(None).map_err(|_| EXIT_CHANNEL)?;
    command
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|_| EXIT_CHANNEL)?;
    Ok((epoch, max_item_bytes))
}

#[cfg(feature = "linux-clipboard")]
mod native {
    use super::{
        Duration, EXIT_CHANNEL, EXIT_PROTOCOL, EXIT_REFUSED, Read, Reply, Request, UnixStream,
        hello, process, receive, send,
    };
    use fr_core::{
        clipboard::{ClipboardSink, PlatformError, process::Change, process::Failure},
        limits::{LimitOverrides, ProtocolLimits},
    };
    use fr_native::{
        clipboard::{ReadError, SynchronizationError, WatchError, X11Clipboard},
        clock,
    };
    use fr_wire::clipboard::session::synchronize::{NativeClipboard, NativeText};

    /// A payload follows its frame immediately; a stalled sender is a lost frd.
    const PAYLOAD_TIMEOUT: Duration = Duration::from_secs(1);

    /// Private bytes, cleared on drop.
    struct Secret(Vec<u8>);
    impl Drop for Secret {
        fn drop(&mut self) {
            self.0.fill(0);
        }
    }

    pub(super) fn run(mut command: UnixStream) -> i32 {
        let (epoch, max) = match hello(&mut command) {
            Ok(hello) => hello,
            Err(code) => return code,
        };
        let limits = ProtocolLimits::with_overrides(LimitOverrides {
            max_clipboard_item_bytes: Some(max),
            ..LimitOverrides::default()
        });
        // Only the explicit local display frd selected; no ambient fallback.
        let opened = match (std::env::var("DISPLAY"), limits) {
            (Ok(display), Ok(limits)) => X11Clipboard::open(&display, &limits, true),
            _ => Err(PlatformError::Unsupported),
        };
        let mut clipboard = match opened {
            Ok(clipboard) => clipboard,
            Err(error) => {
                let _ = send(&mut command, 1, Reply::Refused(error), &[]);
                return EXIT_REFUSED;
            }
        };
        if send(&mut command, 1, Reply::Ready { epoch }, &[]).is_err() {
            return EXIT_CHANNEL;
        }
        let code = serve(&mut command, &mut clipboard, max);
        // Erase private copies and close X11 on every exit path; no selection
        // reset is sent, so another application's newer copy is untouched.
        NativeClipboard::close(&mut clipboard);
        code
    }

    fn serve(command: &mut UnixStream, clipboard: &mut X11Clipboard, max: u32) -> i32 {
        let mut expected = 2_u64;
        loop {
            // EOF (frd gone or closed) ends here.
            let Ok(frame) = receive(command) else {
                return EXIT_CHANNEL;
            };
            let Ok((sequence, request)) = process::decode_request(&frame, max) else {
                return EXIT_PROTOCOL;
            };
            if sequence != expected {
                return EXIT_PROTOCOL;
            }
            let Some(next) = sequence.checked_add(1) else {
                return EXIT_PROTOCOL;
            };
            expected = next;
            let mut payload = Secret(Vec::new());
            let reply = match request {
                Request::Hello { .. } => return EXIT_PROTOCOL,
                Request::Stop => {
                    NativeClipboard::close(clipboard);
                    let _ = send(command, sequence, Reply::Stopped, &[]);
                    return 0;
                }
                Request::Prepare {
                    stamp,
                    revision,
                    len,
                } => match prepare(command, clipboard, stamp, revision, len) {
                    Ok(reply) => reply,
                    Err(code) => return code,
                },
                Request::Publish {
                    stamp,
                    not_after_ns,
                } => {
                    // The final executor check, immediately before XSetSelectionOwner.
                    let publication = clipboard.publish_checked(stamp, || {
                        clock::monotonic_ns().is_some_and(|now| now < not_after_ns)
                    });
                    Reply::Published {
                        revision: clipboard.change_revision(),
                        publication,
                    }
                }
                Request::CancelPrepared => {
                    ClipboardSink::cancel_prepared(clipboard);
                    done(clipboard, Ok(()))
                }
                Request::Watch => match NativeClipboard::watch(clipboard) {
                    Ok(revision) => Reply::Watching { revision },
                    Err(error) => failed(clipboard, error),
                },
                Request::Changes => match NativeClipboard::changes(clipboard) {
                    Ok(changes) => Reply::Changes {
                        revision: clipboard.change_revision(),
                        latest: changes.latest.map(|change| Change {
                            revision: change.revision,
                            has_selection: change.has_selection,
                            origin: change.origin,
                        }),
                        settled: changes.settled,
                    },
                    Err(error) => failed(clipboard, error),
                },
                Request::BeginRead => {
                    let result = NativeClipboard::begin_read(clipboard);
                    done(clipboard, result)
                }
                Request::PollRead => match poll_read(clipboard, max, &mut payload) {
                    Ok(reply) => reply,
                    Err(code) => return code,
                },
                Request::CancelRead => {
                    NativeClipboard::cancel_read(clipboard);
                    done(clipboard, Ok(()))
                }
                Request::Suspend => {
                    let result = NativeClipboard::suspend(clipboard);
                    done(clipboard, result)
                }
            };
            if send(command, sequence, reply, &payload.0).is_err() {
                return EXIT_CHANNEL;
            }
        }
    }
    /// Read exactly the announced payload (its length was checked against the
    /// agreed bound by decoding), then prepare it. `Err` is an exit code.
    fn prepare(
        command: &mut UnixStream,
        clipboard: &mut X11Clipboard,
        stamp: fr_core::clipboard::Stamp,
        revision: Option<u64>,
        len: u32,
    ) -> Result<Reply, i32> {
        let mut text = Secret(Vec::new());
        text.0
            .try_reserve_exact(len as usize)
            .map_err(|_| EXIT_CHANNEL)?;
        text.0.resize(len as usize, 0);
        if command.set_read_timeout(Some(PAYLOAD_TIMEOUT)).is_err()
            || command.read_exact(&mut text.0).is_err()
            || command.set_read_timeout(None).is_err()
        {
            return Err(EXIT_CHANNEL);
        }
        let text = std::str::from_utf8(&text.0).map_err(|_| EXIT_PROTOCOL)?;
        let prepared = match revision {
            Some(revision) => clipboard.prepare_for_revision(text, stamp, revision),
            None => ClipboardSink::prepare(clipboard, text, stamp),
        };
        let revision = clipboard.change_revision();
        Ok(match prepared {
            Ok(()) => Reply::Prepared { revision },
            Err(error) => Reply::PrepareFailed { revision, error },
        })
    }
    /// One bounded native read step; a complete item is copied into `payload`
    /// (never truncated: the native limit already matches the agreed bound).
    fn poll_read(
        clipboard: &mut X11Clipboard,
        max: u32,
        payload: &mut Secret,
    ) -> Result<Reply, i32> {
        Ok(match NativeClipboard::poll_read(clipboard) {
            Ok(None) => Reply::ReadPending {
                revision: clipboard.change_revision(),
            },
            Ok(Some(text)) => {
                let bytes = text.text().as_bytes();
                let len = u32::try_from(bytes.len()).map_err(|_| EXIT_PROTOCOL)?;
                if len > max || payload.0.try_reserve_exact(bytes.len()).is_err() {
                    return Err(EXIT_PROTOCOL);
                }
                payload.0.extend_from_slice(bytes);
                Reply::ReadText {
                    revision: clipboard.change_revision(),
                    origin: text.origin(),
                    len,
                }
            }
            Err(error) => failed(clipboard, error),
        })
    }
    fn done(clipboard: &X11Clipboard, result: Result<(), SynchronizationError>) -> Reply {
        match result {
            Ok(()) => Reply::Done {
                revision: clipboard.change_revision(),
            },
            Err(error) => failed(clipboard, error),
        }
    }
    fn failed(clipboard: &X11Clipboard, error: SynchronizationError) -> Reply {
        Reply::Failed {
            revision: clipboard.change_revision(),
            failure: failure(error),
        }
    }
    const fn failure(error: SynchronizationError) -> Failure {
        match error {
            SynchronizationError::Watch(error) => match error {
                WatchError::Platform(error) => Failure::Platform(error),
                WatchError::Unsupported => Failure::Unsupported,
                WatchError::NotWatching => Failure::NotWatching,
                WatchError::AlreadyWatching => Failure::AlreadyWatching,
                WatchError::GenerationExhausted => Failure::Exhausted,
            },
            SynchronizationError::Read(error) => match error {
                ReadError::Platform(error) => Failure::Platform(error),
                ReadError::Busy => Failure::Busy,
                ReadError::NotReading => Failure::NotReading,
                ReadError::NoSelection => Failure::NoSelection,
                ReadError::Expired => Failure::Expired,
                ReadError::LocalChanged => Failure::LocalChanged,
                ReadError::Limit => Failure::Limit,
                ReadError::Allocation => Failure::Allocation,
                ReadError::Unsupported => Failure::Unsupported,
                ReadError::InvalidUtf8 => Failure::InvalidUtf8,
                ReadError::Malformed => Failure::Malformed,
            },
        }
    }
}

/// Serve one clipboard lane. Without the `linux-clipboard` build the role
/// exists only to refuse typed (`Unsupported`), never to pretend.
pub(super) fn run(command: UnixStream) -> i32 {
    #[cfg(feature = "linux-clipboard")]
    {
        native::run(command)
    }
    #[cfg(not(feature = "linux-clipboard"))]
    {
        let mut command = command;
        if let Err(code) = hello(&mut command) {
            return code;
        }
        let _ = send(
            &mut command,
            1,
            Reply::Refused(fr_core::clipboard::PlatformError::Unsupported),
            &[],
        );
        EXIT_REFUSED
    }
}
