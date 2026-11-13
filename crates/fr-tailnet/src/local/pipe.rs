//! The installed Windows daemon's `LocalAPI`: `tailscaled`'s named pipe.
//!
//! Trust is where the pipe lives. `tailscaled` serves
//! `\\.\pipe\ProtectedPrefix\Administrators\Tailscale\tailscaled`, and only
//! members of the Administrators group (and `SYSTEM`) can create a pipe
//! under `ProtectedPrefix\Administrators`, so an unprivileged process cannot
//! stand in for the daemon. Tailscale's own clients rely on the same rule.
//! This is the counterpart of the Unix socket's root-UID check (`socket.rs`):
//! the host OS and its administrators remain trusted. The server's
//! kernel-reported process ID is the [`SourceIdentity`] every read of one
//! lookup must share, so a daemon restart between reads breaks the lookup's
//! snapshot.
//!
//! The client connects at Identification level: `tailscaled` learns which
//! user asks (its `LocalAPI` permissions depend on that) and cannot act as
//! that user. The pipe I/O is overlapped and wakes the task from the system
//! thread pool (`fr_winsys::pipe`), so a lookup under restricted
//! capabilities never needs a blocking thread.
//!
//! The daemon offers the next pipe instance only after accepting a client,
//! so an open right after another one's can find every instance taken
//! (`ERROR_PIPE_BUSY`; one second open in five on a Windows 11 laptop, 11-12).
//! Such an open is retried every 10 ms, as go-winio's dialer under
//! Tailscale's own client does, but by sleeping the task, never a thread.
use super::{SourceIdentity, checked};
use crate::Error;
use asupersync::{
    cx::Cx,
    http::h1::{Http1Client, Request},
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::sleep,
};
use fr_winsys::pipe::PipeClient;
use std::{
    io,
    path::Path,
    pin::Pin,
    task::{Context, Poll, ready},
    time::Duration,
};

/// The installed daemon's pipe (Tailscale's Windows default).
pub(super) const INSTALLED: &str = r"\\.\pipe\ProtectedPrefix\Administrators\Tailscale\tailscaled";
const ERROR_PIPE_BUSY: i32 = 231;
/// Up to one second for a free instance, in 10 ms steps.
const BUSY_PAUSE: Duration = Duration::from_millis(10);
const BUSY_ATTEMPTS: u32 = 100;

/// Opens the pipe, waiting out instances that are all taken. Without a
/// runtime context to sleep in, a busy pipe is unavailable at once.
async fn open(path: &Path) -> Result<PipeClient, Error> {
    for _ in 1..BUSY_ATTEMPTS {
        match PipeClient::open(path) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                let Some(cx) = Cx::current() else { break };
                sleep(cx.now(), BUSY_PAUSE).await;
            }
            opened => return opened.map_err(|_| Error::LocalApiUnavailable),
        }
    }
    PipeClient::open(path).map_err(|_| Error::LocalApiUnavailable)
}

/// One read of an exact local URI, accepting only a complete 200 response of
/// `content_type` within `maximum`.
pub(super) async fn get(
    path: &Path,
    uri: &str,
    maximum: usize,
    content_type: &str,
) -> Result<(Vec<u8>, SourceIdentity), Error> {
    let pipe = open(path).await?;
    let pid = SourceIdentity::try_from(pipe.server_process_id())
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or(Error::UntrustedLocalApi)?;
    let request = Request::get(uri)
        .header("Host", "local-tailscaled.sock")
        .header("Connection", "close")
        .header("Accept", content_type)
        .build();
    let (response, _pipe, withheld) =
        Http1Client::request_with_io_and_max_body_size(Pipe(pipe), request, maximum)
            .await
            .map_err(|_| Error::Http)?;
    checked(response, withheld, content_type).map(|body| (body, pid))
}

/// The pipe as an Asupersync stream.
struct Pipe(PipeClient);

impl AsyncRead for Pipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let read = ready!(self.get_mut().0.poll_read(cx, buf.unfilled()))?;
        buf.advance(read);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Pipe {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().0.poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().0.poll_flush(cx)
    }
    /// The daemon closes after its reply (`Connection: close`); the pipe
    /// has no half-close, so shutting down only finishes the last write.
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().0.poll_flush(cx)
    }
}

#[cfg(test)]
mod tests;
