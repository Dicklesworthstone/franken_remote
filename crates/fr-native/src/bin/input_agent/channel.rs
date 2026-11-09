//! The private launch channels every executor role shares: frd's command
//! stream on stdin and its signal datagram socket on stdout, with the frame
//! and fence helpers. Platform-neutral std I/O; no native input here.
use fr_core::input_submission::{
    Submission,
    process::{self, Reply},
};
use std::{
    io::{self, Read, Write},
    os::{
        fd::AsFd,
        unix::net::{UnixDatagram, UnixStream},
    },
    time::Duration,
};

pub(crate) const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const SIGNALS_PER_CHECK: usize = 8;
pub(crate) const EXIT_USAGE: i32 = 64;
pub(crate) const EXIT_PROTOCOL: i32 = 65;
pub(crate) const EXIT_REFUSED: i32 = 66;
pub(crate) const EXIT_CHANNEL: i32 = 67;

pub(crate) fn command_channel() -> io::Result<UnixStream> {
    let stream = UnixStream::from(io::stdin().as_fd().try_clone_to_owned()?);
    // Only a connected private socket is accepted, never a file or tty.
    stream.peer_addr()?;
    Ok(stream)
}
pub(crate) fn signal_channel() -> io::Result<UnixDatagram> {
    let socket = UnixDatagram::from(io::stdout().as_fd().try_clone_to_owned()?);
    socket.peer_addr()?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}
pub(crate) fn send(command: &mut UnixStream, sequence: u64, reply: Reply) -> io::Result<()> {
    let frame = process::encode_reply(sequence, reply)
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    command.write_all(&frame)
}
pub(crate) fn receive(command: &mut UnixStream) -> io::Result<[u8; process::FRAME_BYTES]> {
    let mut frame = [0; process::FRAME_BYTES];
    command.read_exact(&mut frame)?;
    Ok(frame)
}
pub(crate) fn reply(submission: Submission) -> Reply {
    match submission {
        Submission::Submitted => Reply::Submitted,
        Submission::NotSubmitted(error) => Reply::NotSubmitted(error),
        Submission::Unknown => Reply::Unknown,
        Submission::Expired => Reply::Expired,
        Submission::Fenced => Reply::Fenced,
    }
}

/// Bounded, nonblocking negative authority. Split from the executor borrow
/// so the final check can run while its original native owner is borrowed.
/// A Fence datagram fences. So does anything else on the private channel
/// (malformed, wrong direction) or a broken channel: the executor fails
/// closed and never reopens.
pub(crate) fn drain_signals(signals: &UnixDatagram, fenced: &mut bool) {
    let mut buffer = [0; process::SIGNAL_BYTES + 1];
    for _ in 0..SIGNALS_PER_CHECK {
        match signals.recv(&mut buffer) {
            Ok(_) => *fenced = true,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                *fenced = true;
                return;
            }
        }
    }
}
