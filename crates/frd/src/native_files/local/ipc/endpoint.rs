//! Native Unix transport for the same bounded, instance-fenced file protocol.
//! Linux keeps its abstract namespace; macOS publishes under a private user
//! directory. Neither pathname ownership nor a label substitutes for peer UID.
use super::{Error, Scope, check_peer, check_time};
use std::{io, os::unix::net::UnixStream, time::Instant};

#[cfg(target_os = "linux")]
pub(super) type Listener = std::os::unix::net::UnixListener;
#[cfg(target_os = "macos")]
pub(super) use pathname::Listener;

#[cfg(any(target_os = "macos", test))]
mod pathname;

pub(super) fn bind(
    scope: Scope,
    uid: u32,
    target: &str,
    until: Instant,
) -> Result<Listener, Error> {
    check_time(until)?;
    #[cfg(target_os = "linux")]
    let result = {
        use std::os::{linux::net::SocketAddrExt, unix::net::SocketAddr};
        let address = SocketAddr::from_abstract_name(super::name(scope, uid, target))
            .map_err(|_| Error::InvalidTarget)?;
        Listener::bind_addr(&address)
    };
    #[cfg(target_os = "macos")]
    let result = pathname::bind(&pathname::address(scope, uid, target), uid, until);
    let listener = match result {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            // A label, occupied socket or held publication lock does not prove
            // another file owner. Check the actual peer before naming one.
            let existing = connect(scope, uid, target, until)?;
            check_peer(&existing, uid)?;
            return Err(Error::AlreadyRunning);
        }
        Err(error) => return Err(failure(&error)),
    };
    check_time(until)?;
    Ok(listener)
}

pub(super) fn connect(
    scope: Scope,
    uid: u32,
    target: &str,
    until: Instant,
) -> Result<UnixStream, Error> {
    check_time(until)?;
    #[cfg(target_os = "linux")]
    let stream = {
        use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
        let socket = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        )
        .map_err(|_| Error::Unavailable)?;
        let address = SocketAddrUnix::new_abstract_name(&super::name(scope, uid, target))
            .map_err(|_| Error::InvalidTarget)?;
        rustix::net::connect(&socket, &address).map_err(|error| match error {
            rustix::io::Errno::CONNREFUSED | rustix::io::Errno::NOENT => Error::NoSession,
            rustix::io::Errno::AGAIN | rustix::io::Errno::INPROGRESS => Error::Busy,
            _ => Error::Unavailable,
        })?;
        UnixStream::from(socket)
    };
    #[cfg(target_os = "macos")]
    let stream = pathname::connect(&pathname::address(scope, uid, target), uid, until)
        .map_err(|error| failure(&error))?;
    check_time(until)?;
    Ok(stream)
}

pub(super) fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    #[cfg(target_os = "linux")]
    {
        rustix::net::sockopt::socket_peercred(stream)
            .map(|credentials| credentials.uid.as_raw())
            .map_err(Into::into)
    }
    #[cfg(target_os = "macos")]
    {
        // The pinned safe adapter uses getpeereid on macOS. It makes the
        // socket nonblocking (as these command connections require). This
        // temporary duplicate never registers with a reactor, reads protocol
        // bytes or takes the original socket owner.
        asupersync::net::unix::UnixStream::from_std(stream.try_clone()?)?
            .peer_cred()
            .map(|credentials| credentials.uid)
    }
}

fn failure(error: &io::Error) -> Error {
    match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => Error::NoSession,
        io::ErrorKind::PermissionDenied => Error::UntrustedPeer,
        io::ErrorKind::InvalidInput => Error::InvalidTarget,
        io::ErrorKind::TimedOut => Error::Timeout,
        io::ErrorKind::WouldBlock => Error::Busy,
        _ => Error::Unavailable,
    }
}
