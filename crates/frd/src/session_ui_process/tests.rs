//! Real private channel and launch-environment checks. The child has no UI.
use super::*;
use rustix::fs::{OFlags, fcntl_getfl};
use std::io::Read;

#[test]
fn selected_platform_context_and_private_channels_survive_exact_child_exit() {
    const FIXTURE: &str = r"#!/usr/bin/python3
import os, socket, sys
assert sys.argv[1:] == ['--parent-pid', str(os.getppid())]
assert 'HOME' not in os.environ and 'PATH' not in os.environ
if @MACOS@:
    assert 'DISPLAY' not in os.environ and 'XAUTHORITY' not in os.environ
else:
    assert os.environ['DISPLAY'] == ':17.1'
    assert os.environ['XAUTHORITY'] == '/qualified/private/Xauthority'
command = socket.socket(fileno=0)
signal = socket.socket(fileno=1)
assert command.type == socket.SOCK_STREAM
assert signal.type == socket.SOCK_DGRAM
command.sendall(b'ready')
signal.send(b'signal')
";
    let path = std::env::temp_dir().join(format!("fr-session-ui-launch-{}.py", std::process::id()));
    crate::executable::write(
        &path,
        FIXTURE.replace(
            "@MACOS@",
            if cfg!(target_os = "macos") {
                "True"
            } else {
                "False"
            },
        ),
    );
    let (display, authority) = if cfg!(target_os = "macos") {
        ("", None)
    } else {
        (
            ":17.1",
            Some(std::path::Path::new("/qualified/private/Xauthority")),
        )
    };
    let launch = ProcessLaunch::new(&path, display, authority, 7).unwrap();
    let (mut child, mut command, signals) = spawn(&launch).unwrap();
    assert!(fcntl_getfl(&command).unwrap().contains(OFlags::NONBLOCK));
    assert!(fcntl_getfl(&signals).unwrap().contains(OFlags::NONBLOCK));
    assert!(child.wait().unwrap().success(), "child launch contract");
    let mut ready = [0; 5];
    command.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready");
    let mut signal = [0; 7];
    assert_eq!(signals.recv(&mut signal).unwrap(), 6);
    assert_eq!(&signal[..6], b"signal");
    // Dropping the Command in production closes the parent's duplicate child
    // endpoints. The original child's exit must be observable as immediate EOF.
    assert_eq!(command.read(&mut [0]).unwrap(), 0);
}
