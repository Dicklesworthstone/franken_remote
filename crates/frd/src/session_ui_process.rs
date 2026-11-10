//! Shared private launcher for the installed session-UI image. Call only from
//! an owned native-I/O worker, never a reactor or an authority callback. The
//! first private record selects a role; no role or authority travels in argv.
use crate::input_process::ProcessLaunch;
use std::{
    io,
    os::{
        fd::OwnedFd,
        unix::{
            net::{UnixDatagram, UnixStream},
            process::CommandExt,
        },
    },
    process::{Child, Command, Stdio},
};

pub(crate) fn spawn(launch: &ProcessLaunch) -> io::Result<(Child, UnixStream, UnixDatagram)> {
    let (command, child_command) = UnixStream::pair()?;
    let (signals, child_signals) = UnixDatagram::pair()?;
    command.set_nonblocking(true)?;
    signals.set_nonblocking(true)?;
    let mut builder = Command::new(&launch.image);
    builder.env_clear();
    // The macOS role inherits the user's actual login-session bootstrap;
    // it has no X11 display or authority. Match the input executor's launch
    // contract instead of creating a misleading empty DISPLAY variable.
    if !launch.display.is_empty() {
        builder.env("DISPLAY", &launch.display);
    }
    builder
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .stdin(Stdio::from(OwnedFd::from(child_command)))
        .stdout(Stdio::from(OwnedFd::from(child_signals)))
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(path) = &launch.xauthority {
        builder.env("XAUTHORITY", path);
    }
    let child = builder.spawn()?;
    // No parent-held duplicate of the child's ends may mask EOF.
    drop(builder);
    Ok((child, command, signals))
}

#[cfg(test)]
#[path = "session_ui_process/tests.rs"]
mod tests;
