//! The installed daemon's Unix-socket `LocalAPI`: Linux's only transport, and
//! on macOS the open-source `tailscaled`'s (Homebrew or source builds; the
//! standalone app serves loopback TCP instead, see `macos/endpoint.rs`).
//!
//! Trust is the kernel's report of the connected server, never the path: its
//! UID must be the daemon's (root) even at a custom path, so a socket an
//! unprivileged process bound is refused before any request is written. The
//! server's PID is the [`SourceIdentity`] every read of one lookup must share,
//! so a daemon restart between reads breaks the lookup's snapshot.
use super::{SourceIdentity, checked};
use crate::Error;
use asupersync::{
    http::h1::{Http1Client, Request},
    net::unix::UnixStream,
};
use std::path::Path;

/// `sun_path` capacity including its terminating NUL.
#[cfg(target_os = "linux")]
const SUN_PATH: usize = 108;
#[cfg(target_os = "macos")]
const SUN_PATH: usize = 104;

/// The open-source macOS daemon's fixed socket (`tailscaled`'s darwin
/// default), in the root-owned `/var/run`. Read only when the standalone app
/// has published no endpoint (see `LocalApi::installed`).
#[cfg(target_os = "macos")]
pub(super) const MACOS_INSTALLED: &str = "/var/run/tailscaled.socket";

/// A locally configured socket path: absolute, no `..`, no NUL, and short
/// enough for `sun_path`. Says nothing about who serves it.
pub(super) fn valid_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    path.is_absolute()
        && bytes.len() < SUN_PATH
        && !bytes.contains(&0)
        && !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// One read of an exact local URI, accepting only a complete 200 response of
/// `content_type` within `maximum`, from a server the kernel reports as
/// `daemon_uid`.
pub(super) async fn get(
    path: &Path,
    daemon_uid: u32,
    uri: &str,
    maximum: usize,
    content_type: &str,
) -> Result<(Vec<u8>, SourceIdentity), Error> {
    let socket = UnixStream::connect(path)
        .await
        .map_err(|_| Error::LocalApiUnavailable)?;
    let pid = daemon_pid(&socket, daemon_uid)?;
    let request = Request::get(uri)
        .header("Host", "local-tailscaled.sock")
        .header("Connection", "close")
        .header("Accept", content_type)
        .build();
    let (response, _socket, withheld) =
        Http1Client::request_with_io_and_max_body_size(socket, request, maximum)
            .await
            .map_err(|_| Error::Http)?;
    checked(response, withheld, content_type).map(|body| (body, pid))
}

/// `SO_PEERCRED`: UID and PID in one kernel record.
#[cfg(target_os = "linux")]
fn daemon_pid(socket: &UnixStream, daemon_uid: u32) -> Result<SourceIdentity, Error> {
    let credentials = socket.peer_cred().map_err(|_| Error::UntrustedLocalApi)?;
    if credentials.uid != daemon_uid || credentials.pid.is_none_or(|pid| pid <= 0) {
        return Err(Error::UntrustedLocalApi);
    }
    credentials.pid.ok_or(Error::UntrustedLocalApi)
}

/// XNU has no `SO_PEERCRED`. Asupersync reads the UID with `getpeereid` and
/// reports no PID, so the PID comes from `LOCAL_PEERPID` on the same
/// connected socket: the kernel's record of the server process, the exact
/// counterpart of Linux's PID (Asupersync already builds `nix` with this
/// option, so no new code enters the graph). It was chosen over the socket
/// file's (device, inode): that names a path rather than the process that
/// answered, can only be compared by a `stat` racing the `connect`, and stays
/// the same when a listening socket outlives its daemon (socket activation).
/// UID and PID both describe the connected server, never a path. XNU PIDs are
/// at most 99,999 and app endpoint identities at least 2^30
/// (`macos/endpoint.rs`), so reads from the two transports never agree.
#[cfg(target_os = "macos")]
fn daemon_pid(socket: &UnixStream, daemon_uid: u32) -> Result<SourceIdentity, Error> {
    use nix::sys::socket::{getsockopt, sockopt::LocalPeerPid};
    let credentials = socket.peer_cred().map_err(|_| Error::UntrustedLocalApi)?;
    if credentials.uid != daemon_uid {
        return Err(Error::UntrustedLocalApi);
    }
    let pid = getsockopt(socket.as_std(), LocalPeerPid).map_err(|_| Error::UntrustedLocalApi)?;
    if pid <= 0 {
        return Err(Error::UntrustedLocalApi);
    }
    Ok(pid)
}
