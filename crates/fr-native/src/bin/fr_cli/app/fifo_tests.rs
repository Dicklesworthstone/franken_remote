use super::*;
use fr_client::completion::CompletionError;
use std::{
    io::Read,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    time::Instant,
};

const END: &str = r#"{"outcome":"success","platform":"macos","ended":"window_closed","local_cleanup_confirmed":true}"#;
const SHORT: Duration = Duration::from_millis(50);

fn command(owner: &PrivatePipe, script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", script, "fr-app-fifo-test"])
        .arg(&owner.path)
        .arg(END)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    command
}

#[test]
fn fifo_and_its_parent_are_private_and_removed_with_their_owner() {
    let owner = PrivatePipe::new().unwrap();
    let directory = owner.directory.clone();
    let path = owner.path.clone();
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let pipe = std::fs::metadata(&path).unwrap();
    assert!(pipe.file_type().is_fifo());
    assert_eq!(pipe.permissions().mode() & 0o777, 0o600);
    drop(owner);
    assert!(!directory.exists());
    assert!(!path.exists());
}

#[test]
fn fifo_drains_a_live_writer_continuously_past_pipe_capacity() {
    let owner = PrivatePipe::new().unwrap();
    let reader = owner.reader().unwrap();
    let child = command(&owner, "exec > \"$1\"; i=0; while [ \"$i\" -lt 32768 ]; do printf '%064d\\n' 0; i=$((i+1)); done; printf '%s\\n' \"$2\"")
        .spawn().unwrap();
    let output = follow(child, reader, Some(START_TIMEOUT), DRAIN_TIMEOUT).unwrap();
    assert!(output.status.success());
    assert!(output.stdout.report.bytes > 2 * 1024 * 1024);
    assert_eq!(output.stdout.report.lines, 32769);
    assert_eq!(output.stdout.completion(), Ok(END));
}

#[test]
fn failed_launcher_without_a_writer_returns_without_waiting_for_fifo_open() {
    let owner = PrivatePipe::new().unwrap();
    let reader = owner.reader().unwrap();
    let child = command(&owner, "exit 9").spawn().unwrap();
    let start = Instant::now();
    let output = follow(child, reader, Some(START_TIMEOUT), DRAIN_TIMEOUT).unwrap();
    assert_eq!(output.status.code(), Some(9));
    assert_eq!(output.stdout.completion(), Err(CompletionError::Missing));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn launcher_that_never_opens_stdout_is_stopped_and_reaped() {
    let owner = PrivatePipe::new().unwrap();
    let reader = owner.reader().unwrap();
    let child = command(&owner, "exec sleep 30").spawn().unwrap();
    let pid = child.id();
    let start = Instant::now();
    assert!(matches!(
        follow(child, reader, Some(SHORT), SHORT),
        Err(Error::Capture)
    ));
    assert!(start.elapsed() < Duration::from_secs(5));
    super::super::tests::assert_reaped(pid);
}

#[test]
fn closing_stdout_without_process_exit_cannot_hang_or_reuse_its_report() {
    let owner = PrivatePipe::new().unwrap();
    let reader = owner.reader().unwrap();
    let child = command(
        &owner,
        "exec > \"$1\"; printf '%s\\n' \"$2\"; exec 1>&-; exec sleep 30",
    )
    .spawn()
    .unwrap();
    let pid = child.id();
    assert!(matches!(
        follow(child, reader, Some(SHORT), SHORT),
        Err(Error::Capture)
    ));
    super::super::tests::assert_reaped(pid);
}

#[test]
fn a_writer_remaining_after_launcher_exit_has_a_bounded_drain() {
    let owner = PrivatePipe::new().unwrap();
    let reader = owner.reader().unwrap();
    let mut writer = command(&owner, "exec 3> \"$1\"; printf x; exec sleep 30")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = [0];
    writer
        .stdout
        .take()
        .unwrap()
        .read_exact(&mut ready)
        .unwrap();
    assert_eq!(ready, [b'x']);
    let waiter = command(&owner, "exit 0").spawn().unwrap();
    let result = follow(waiter, reader, Some(SHORT), SHORT);
    writer.kill().unwrap();
    writer.wait().unwrap();
    assert!(matches!(result, Err(Error::Capture)));
}
