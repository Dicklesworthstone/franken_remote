use super::*;
use crate::session_output::{
    COMPLETION_LINE_LIMIT, capture_completion,
    child::{FollowError, StatusCapture, Stream},
    drain_status,
};
use std::{
    io::{self, Cursor, Read},
    process::ExitStatus,
};

const REPORT: &[u8] = br#"{"status":"ended","end":"ok","drawn":12,"dxva":1,"seconds":9,"reopens":0,"reopen_refusal":null,"clipboard":{"requested":false,"configured":false,"native_started":false,"received_submitted":0,"received_unknown":0,"received_refused":0,"cleanup":"not_started","reason":null},"upload":null,"download":null}"#;

fn report(end: &str, drawn: u64) -> String {
    let mut report: serde_json::Value = serde_json::from_slice(REPORT).unwrap();
    report["end"] = end.into();
    report["drawn"] = drawn.into();
    report.to_string()
}

fn exit(code: u8) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        ExitStatus::from_raw(i32::from(code) << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt as _;
        ExitStatus::from_raw(u32::from(code))
    }
}

fn status(reader: impl Read) -> StatusCapture {
    let mut first_line = None;
    let report = drain_status(reader, |line, _| {
        if first_line.is_none() && !line.iter().all(u8::is_ascii_whitespace) {
            first_line = Some(line.to_vec());
        }
    });
    StatusCapture {
        report,
        first_line,
        log_error: None,
    }
}

fn output(stdout: &[u8], stderr: &[u8], code: u8) -> ChildOutput {
    ChildOutput {
        status: Some(exit(code)),
        stdout: Some(capture_completion(Cursor::new(stdout))),
        stderr: Some(status(Cursor::new(stderr))),
        ..ChildOutput::default()
    }
}

#[test]
fn a_complete_success_preserves_the_actual_session_summary() {
    let output = output(REPORT, b"", 0);
    let summary = summarize(&output, "studio", false);
    assert!(summary.ok);
    assert_eq!(
        summary.continuation,
        Continuation::Picker { expected_exit: 0 }
    );
    assert_eq!(
        summary,
        outcome::summarize(std::str::from_utf8(REPORT).unwrap(), "studio")
    );
}

#[test]
fn a_complete_host_refusal_is_never_a_positive_notice() {
    let report = report("host_busy", 0);
    let output = output(report.as_bytes(), b"", 1);
    let summary = summarize(&output, "studio", false);
    assert!(!summary.ok);
    assert_eq!(
        summary.continuation,
        Continuation::Picker { expected_exit: 1 }
    );
    assert_eq!(summary, outcome::summarize(&report, "studio"));
}

#[test]
fn normal_refusal_and_zero_picture_exit_one_are_not_process_crashes() {
    for report in [
        report("ok", 0),
        report("cancelled", 0),
        report("host_busy", 0),
        r#"{"status":"refused","reason":"no_d3d11_device"}"#.into(),
    ] {
        let captured = output(report.as_bytes(), b"", 1);
        let summary = summarize(&captured, "studio", false);
        assert!(!summary.ok);
        assert_eq!(
            summary.continuation,
            Continuation::Picker { expected_exit: 1 },
            "{report}"
        );
        assert!(!summary.message.contains("exited with code"));
        let warning = summarize(&captured, "studio", true);
        assert_eq!(warning.continuation, summary.continuation);
        assert!(warning.message.contains("local session log"));

        for exit_code in [0, 2, 7] {
            let summary = summarize(&output(report.as_bytes(), b"", exit_code), "studio", false);
            assert_eq!(
                summary.continuation,
                Continuation::Stop,
                "{report}, exit {exit_code}"
            );
        }
    }
}

#[test]
fn a_confirmed_refusal_cannot_hide_incomplete_original_cleanup() {
    let mut captured = output(report("host_busy", 0).as_bytes(), b"", 1);
    captured.cleanup_error = Some(io::ErrorKind::PermissionDenied);
    assert_eq!(
        summarize(&captured, "studio", false).continuation,
        Continuation::Stop
    );
}

#[cfg(unix)]
#[test]
fn actual_child_completion_and_exit_jointly_control_the_next_picker() {
    use crate::session_output::child::follow_child;
    use std::process::{Command, Stdio};

    let success = std::str::from_utf8(REPORT).unwrap().to_owned();
    for (report, code, expected) in [
        (
            success.clone(),
            0,
            Continuation::Picker { expected_exit: 0 },
        ),
        (success.clone(), 1, Continuation::Stop),
        (
            report("cancelled", 0),
            1,
            Continuation::Picker { expected_exit: 1 },
        ),
        (
            report("host_busy", 0),
            1,
            Continuation::Picker { expected_exit: 1 },
        ),
        (report("host_busy", 0), 0, Continuation::Stop),
        (
            r#"{"status":"refused","reason":"window_unavailable"}"#.into(),
            1,
            Continuation::Picker { expected_exit: 1 },
        ),
        (
            r#"{"status":"refused","reason":"swap_chain_unavailable"}"#.into(),
            1,
            Continuation::Stop,
        ),
        (r#"{"end":"ok","drawn":12}"#.into(), 0, Continuation::Stop),
        (
            format!("{success}\nnot a terminal report"),
            0,
            Continuation::Stop,
        ),
        (String::new(), 0, Continuation::Stop),
    ] {
        // Positional arguments carry fixture data; the shell only writes the
        // exact report and exits. This exercises real drainage and reaping.
        let child = Command::new("sh")
            .args([
                "-c",
                "printf '%s\\n' \"$1\"; exit \"$2\"",
                "completion-test",
            ])
            .arg(&report)
            .arg(code.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = follow_child(child, None);
        assert_eq!(output.failure, None);
        assert_eq!(output.cleanup_error, None);
        assert_eq!(output.status.unwrap().code(), Some(code));
        let summary = summarize(&output, "studio", false);
        assert_eq!(summary.continuation, expected, "{report}, exit {code}");
    }
}

#[test]
fn a_success_document_cannot_hide_a_failed_process_exit() {
    let summary = summarize(&output(REPORT, b"", 7), "studio", false);
    assert!(!summary.ok);
    assert_eq!(summary.continuation, Continuation::Stop);
    assert!(
        summary
            .message
            .contains("Disconnected from studio after 9 s.")
    );
    assert!(summary.message.contains("exited with code 7"));
}

#[cfg(unix)]
#[test]
fn a_signal_exit_is_distinguished_from_a_normal_exit_code() {
    use std::os::unix::process::ExitStatusExt as _;
    let mut output = output(REPORT, b"", 0);
    output.status = Some(ExitStatus::from_raw(9));
    let summary = summarize(&output, "studio", false);
    assert!(!summary.ok);
    assert_eq!(summary.continuation, Continuation::Stop);
    assert!(summary.message.contains("without a normal exit code"));
}

#[test]
fn missing_completion_preserves_first_refusal_guidance_without_inventing_connection_state() {
    let output = output(
        b"",
        b"\r\nfr-windows: The receive directory is unavailable.\nLater status\n",
        2,
    );
    let summary = summarize(&output, "studio", false);
    assert!(!summary.ok);
    assert_eq!(summary.continuation, Continuation::Stop);
    assert!(
        summary
            .message
            .starts_with("The session outcome for studio is unknown")
    );
    assert!(
        summary
            .message
            .contains("Viewer message: The receive directory is unavailable.")
    );
    assert!(!summary.message.contains("Later status"));
    assert!(!summary.message.contains("Could not connect"));
    assert!(!summary.message.contains("Could not start"));
}

#[test]
fn retained_json_after_oversized_output_is_diagnostic_only() {
    let mut bytes = vec![b'x'; COMPLETION_LINE_LIMIT + 1];
    bytes.push(b'\n');
    bytes.extend_from_slice(REPORT);
    let output = output(&bytes, b"", 0);
    assert!(
        output
            .stdout
            .as_ref()
            .unwrap()
            .retained_document()
            .is_some()
    );
    let summary = summarize(&output, "studio", false);
    assert!(!summary.ok);
    assert_eq!(summary.continuation, Continuation::Stop);
    assert!(summary.message.contains("exceeded the size limit"));
    assert!(!summary.message.contains("Disconnected"));
}

#[test]
fn later_invalid_terminal_output_cannot_reuse_an_earlier_success() {
    for suffix in [b"not a report".as_slice(), b"{\xff}"] {
        let mut bytes = REPORT.to_vec();
        bytes.push(b'\n');
        bytes.extend_from_slice(suffix);
        let summary = summarize(&output(&bytes, b"", 0), "studio", false);
        assert!(!summary.ok);
        assert_eq!(summary.continuation, Continuation::Stop);
        assert!(summary.message.contains("completion output was invalid"));
        assert!(!summary.message.contains("Disconnected"));
    }
}

struct FailAfter(Cursor<Vec<u8>>);

impl Read for FailAfter {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self.0.read(bytes)? {
            0 => Err(io::ErrorKind::BrokenPipe.into()),
            count => Ok(count),
        }
    }
}

#[test]
fn a_read_failure_after_a_complete_json_line_cannot_certify_success() {
    let mut output = output(b"", b"", 0);
    let mut bytes = REPORT.to_vec();
    bytes.push(b'\n');
    output.stdout = Some(capture_completion(FailAfter(Cursor::new(bytes))));
    output.failure = Some(FollowError::Read(Stream::Stdout, io::ErrorKind::BrokenPipe));
    assert!(
        output
            .stdout
            .as_ref()
            .unwrap()
            .retained_document()
            .is_some()
    );
    let summary = summarize(&output, "studio", false);
    assert!(!summary.ok);
    assert_eq!(summary.continuation, Continuation::Stop);
    assert!(
        summary
            .message
            .contains("completion report could not be read")
    );
    assert!(summary.message.contains("could not be followed completely"));
    assert!(!summary.message.contains("Disconnected"));
}

#[test]
fn every_follower_failure_fences_a_positive_notice_but_keeps_received_outcome() {
    for failure in [
        FollowError::MissingPipe,
        FollowError::Spawn(Stream::Stdout, io::ErrorKind::Other),
        FollowError::Read(Stream::Stderr, io::ErrorKind::BrokenPipe),
        FollowError::ReaderPanicked(Stream::Stderr),
        FollowError::ReaderDisconnected,
        FollowError::Wait(io::ErrorKind::Other),
    ] {
        let mut output = output(REPORT, b"", 0);
        output.failure = Some(failure);
        let summary = summarize(&output, "studio", false);
        assert!(!summary.ok, "{failure:?}");
        assert_eq!(summary.continuation, Continuation::Stop, "{failure:?}");
        assert!(summary.message.contains("Disconnected from studio"));
        assert!(summary.message.contains("could not be followed completely"));
    }
}

#[test]
fn cleanup_uncertainty_and_missing_status_remain_explicit() {
    let mut output = output(REPORT, b"", 0);
    output.status = None;
    output.cleanup_error = Some(io::ErrorKind::PermissionDenied);
    let summary = summarize(&output, "studio", false);
    assert!(!summary.ok);
    assert_eq!(summary.continuation, Continuation::Stop);
    assert!(
        summary
            .message
            .contains("Local viewer cleanup could not be confirmed")
    );
    assert!(
        summary
            .message
            .contains("process exit could not be confirmed")
    );
}

#[test]
fn truncated_or_failed_stderr_does_not_hide_a_received_outcome_or_qualify_success() {
    let mut output = output(REPORT, &vec![b'x'; STATUS_LINE_LIMIT + 1], 0);
    let summary = summarize(&output, "studio", false);
    assert!(!summary.ok);
    assert_eq!(summary.continuation, Continuation::Stop);
    assert!(summary.message.contains("Disconnected from studio"));
    assert!(summary.message.contains("diagnostics were truncated"));

    output.stderr = Some(status(FailAfter(Cursor::new(
        b"original guidance\n".to_vec(),
    ))));
    let summary = summarize(&output, "studio", false);
    assert!(!summary.ok);
    assert_eq!(summary.continuation, Continuation::Stop);
    assert!(summary.message.contains("Disconnected from studio"));
    assert!(
        summary
            .message
            .contains("diagnostics could not be read completely")
    );

    output.stderr = None;
    let summary = summarize(&output, "studio", false);
    assert!(!summary.ok);
    assert_eq!(summary.continuation, Continuation::Stop);
    assert!(summary.message.contains("diagnostics were unavailable"));
}

#[test]
fn both_initial_and_midstream_log_failures_preserve_the_actual_outcome() {
    for initial_failure in [false, true] {
        let mut output = output(REPORT, b"", 0);
        if !initial_failure {
            output.stderr.as_mut().unwrap().log_error = Some(io::ErrorKind::WriteZero);
        }
        let summary = summarize(&output, "studio", initial_failure);
        assert!(!summary.ok);
        assert_eq!(
            summary.continuation,
            Continuation::Picker { expected_exit: 0 }
        );
        assert!(
            summary
                .message
                .starts_with("Disconnected from studio after 9 s.")
        );
        assert!(
            summary
                .message
                .contains("local session log could not be saved completely")
        );
        assert!(!summary.message.contains("outcome for studio is unknown"));
    }
}

#[test]
fn malformed_json_is_not_a_success_even_when_capture_and_exit_are_complete() {
    let output = output(b"{not valid json}", b"", 0);
    assert!(output.complete());
    let summary = summarize(&output, "studio", false);
    assert!(!summary.ok);
    assert_eq!(summary.continuation, Continuation::Stop);
    assert!(summary.message.contains("without a report"));
}

#[test]
fn first_guidance_is_bounded_and_cannot_hide_later_warnings_with_control_characters() {
    let mut bytes = b"fr-windows: original\0guidance\x1b".to_vec();
    bytes.extend(vec![b'x'; STATUS_LINE_LIMIT * 2]);
    let summary = summarize(&output(b"", &bytes, 7), "studio", false);
    assert!(!summary.ok);
    assert_eq!(summary.continuation, Continuation::Stop);
    assert!(
        summary
            .message
            .contains("Viewer message: original guidance ")
    );
    assert!(!summary.message.chars().any(char::is_control));
    assert!(summary.message.contains("exited with code 7"));
    assert!(summary.message.contains("diagnostics were truncated"));
    assert!(summary.message.len() < STATUS_LINE_LIMIT + 512);
}
