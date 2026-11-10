//! One bounded clipboard process protocol, independent of the OS owner.
//! Native backends supply only their final submission gate and typed errors.
//! Authority, lease lifetime and record attachment remain in frd.
use super::{
    Duration, EXIT_CHANNEL, EXIT_PROTOCOL, EXIT_REFUSED, Read, Reply, Request, UnixStream,
    process, receive, send,
};
use fr_core::clipboard::{ClipboardSink, Publication, Stamp, process::Change, process::Failure};
use fr_wire::clipboard::session::synchronize::{NativeClipboard, NativeText};

/// A native submission consumes the preparation exactly once. Implementations
/// must check the original monotonic nanosecond deadline at the OS boundary.
pub(super) trait Backend: NativeClipboard {
    fn publish_prepared(
        &mut self,
        stamp: Stamp,
        not_after_ns: u64,
        live: &dyn Fn() -> bool,
    ) -> Publication;
    fn failure(error: Self::Error) -> Failure;
}

/// A payload follows its frame immediately; a stalled sender is a lost frd.
const PAYLOAD_TIMEOUT: Duration = Duration::from_secs(1);

/// Private bytes, cleared on every exit; no content Debug implementation.
struct Secret(Vec<u8>);
impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

pub(super) fn serve<C: Backend>(
    command: &mut UnixStream,
    clipboard: &mut C,
    max: u32,
    live: &dyn Fn() -> bool,
) -> i32 {
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
        // Release-only cleanup remains possible. Any operation that can
        // watch/read/prepare/publish requires the still-live local surface.
        if !matches!(
            request,
            Request::Stop | Request::CancelPrepared | Request::CancelRead | Request::Suspend
        ) && !live()
        {
            return EXIT_REFUSED;
        }
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
            } => match prepare(command, clipboard, stamp, revision, len, live) {
                Ok(reply) => reply,
                Err(code) => return code,
            },
            Request::Publish {
                stamp,
                not_after_ns,
            } => {
                // Each backend rechecks this original deadline and live
                // privacy state immediately before every native mutation.
                let publication = clipboard.publish_prepared(stamp, not_after_ns, live);
                Reply::Published {
                    revision: clipboard.revision(),
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
                    revision: clipboard.revision(),
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
            Request::PollRead => match poll_read(clipboard, max, &mut payload, live) {
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
        // A local stop during a native read erases the private result;
        // no text is returned on the command channel after that stop.
        if !payload.0.is_empty() && !live() {
            return EXIT_REFUSED;
        }
        if send(command, sequence, reply, &payload.0).is_err() {
            return EXIT_CHANNEL;
        }
    }
}
/// Read exactly the announced payload (its length was checked against the
/// agreed bound by decoding), then prepare it. `Err` is an exit code.
fn prepare<C: Backend>(
    command: &mut UnixStream,
    clipboard: &mut C,
    stamp: fr_core::clipboard::Stamp,
    revision: Option<u64>,
    len: u32,
    live: &dyn Fn() -> bool,
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
    if !live() {
        return Err(EXIT_REFUSED);
    }
    let prepared = match revision {
        Some(revision) => clipboard.prepare_for_revision(text, stamp, revision),
        None => ClipboardSink::prepare(clipboard, text, stamp),
    };
    let revision = clipboard.revision();
    Ok(match prepared {
        Ok(()) => Reply::Prepared { revision },
        Err(error) => Reply::PrepareFailed { revision, error },
    })
}
/// One bounded native read step; a complete item is copied into `payload`
/// (never truncated: the native limit already matches the agreed bound).
fn poll_read<C: Backend>(
    clipboard: &mut C,
    max: u32,
    payload: &mut Secret,
    live: &dyn Fn() -> bool,
) -> Result<Reply, i32> {
    Ok(match NativeClipboard::poll_read(clipboard) {
        Ok(None) => Reply::ReadPending {
            revision: clipboard.revision(),
        },
        Ok(Some(text)) => {
            if !live() {
                return Err(EXIT_REFUSED);
            }
            let bytes = text.text().as_bytes();
            let len = u32::try_from(bytes.len()).map_err(|_| EXIT_PROTOCOL)?;
            if len > max || payload.0.try_reserve_exact(bytes.len()).is_err() {
                return Err(EXIT_PROTOCOL);
            }
            payload.0.extend_from_slice(bytes);
            Reply::ReadText {
                revision: clipboard.revision(),
                origin: text.origin(),
                len,
            }
        }
        Err(error) => failed(clipboard, error),
    })
}
fn done<C: Backend>(clipboard: &C, result: Result<(), C::Error>) -> Reply {
    match result {
        Ok(()) => Reply::Done {
            revision: clipboard.revision(),
        },
        Err(error) => failed(clipboard, error),
    }
}
fn failed<C: Backend>(clipboard: &C, error: C::Error) -> Reply {
    Reply::Failed {
        revision: clipboard.revision(),
        failure: C::failure(error),
    }
}

#[cfg(test)]
mod tests;
