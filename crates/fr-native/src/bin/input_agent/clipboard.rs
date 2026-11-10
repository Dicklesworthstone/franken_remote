//! `fr-input-agent --clipboard`: the per-lease out-of-process X11 CLIPBOARD
//! owner for frd's controlled share (frd never loads Xlib/XCB).
//!
//! Launched only by frd's `clipboard_process` factory, on the clipboard worker
//! thread, after the controller's input grant and bilateral clipboard
//! readiness, with a cleared environment (DISPLAY, optional XAUTHORITY),
//! `--parent-pid` and private `SOCK_STREAM` channels: commands on stdin and a
//! content-free clipboard privacy stop channel on stdout. It
//! holds no lease, ticket, peer identity or network socket: frd's synchronizer
//! performs every authority check. This child only executes one bounded native
//! request at a time, and re-checks frd's publication deadline in its own
//! `CLOCK_MONOTONIC` immediately before the X11 ownership call.
//!
//! Payloads are bounded by the item size agreed in `Hello`, checked from the
//! frame before any allocation. On EOF, Stop or a protocol violation it erases
//! its private copies and closes X11 without resetting another application's
//! newer selection. It writes nothing but reply frames and never logs text.
use crate::channel::{EXIT_CHANNEL, EXIT_PROTOCOL, EXIT_REFUSED};
use fr_core::clipboard::process::{self, Reply, Request};
use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    time::Duration,
};

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// frd reads every reply at once; a reader that stops is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(feature = "linux-clipboard")]
#[path = "clipboard/linux.rs"]
mod linux;
#[cfg(feature = "linux-clipboard")]
#[path = "clipboard/privacy.rs"]
mod privacy;
#[cfg(any(feature = "linux-clipboard", test))]
#[path = "clipboard/serve.rs"]
mod serve;

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

/// Serve one clipboard lane. Without the `linux-clipboard` build the role
/// exists only to refuse typed (`Unsupported`), never to pretend.
pub(super) fn run(command: UnixStream) -> i32 {
    #[cfg(feature = "linux-clipboard")]
    {
        linux::run(command)
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
