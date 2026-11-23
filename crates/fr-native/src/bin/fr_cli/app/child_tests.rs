use super::super::{DEFAULT_SERVICE_PORT, Report, last_report, picker_report, session_notice};
use super::*;
use fr_client::completion::{CompletionError, NATIVE_COMPLETION_LINE_LIMIT};
use std::{
    io,
    time::{Duration, Instant},
};

const END: &str =
    r#"{"schema_version":1,"outcome":"stopped","role":"observe","cleanup_confirmed":true}"#;
const SELECTED_PORT: u16 = 9443;
const SELECTED: &str = r#"{"schema_version":1,"event":"host_selected","node_id":"nMAC","certificate_name":"mac.tail.ts.net","port":9443,"control":true,"clipboard":false,"choose_display":false,"playback_audio":false,"send_files":false,"receive_files":false,"microphone_controls":false,"capture_system_shortcuts":false,"selection_only":true,"fresh_lookup_required":true,"remote_session_started":false,"input_requested":false}"#;

fn shell(script: &str, document: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", script, "fr-app-test", document]);
    command
}

#[test]
fn drains_live_stdout_beyond_pipe_capacity_and_retains_only_final_document() {
    let output = run(&mut shell(
        "i=0; while [ \"$i\" -lt 32768 ]; do printf '%064d\\n' 0; i=$((i+1)); done; printf '%s\\n' \"$1\"",
        END,
    )).unwrap();
    assert!(output.status.success());
    assert!(output.stdout.report.bytes > 2 * 1024 * 1024);
    assert_eq!(output.stdout.report.lines, 32769);
    assert_eq!(output.stdout.completion(), Ok(END));
    assert_eq!(last_report(&output.stdout), Some(Report::Ended));
    assert!(
        session_notice(last_report(&output.stdout), "mac", output.status.success())
            .unwrap()
            .contains("local cleanup was confirmed")
    );
}

#[test]
fn oversized_live_output_is_fully_drained_but_cannot_be_success() {
    let output = run(&mut shell(
        "dd if=/dev/zero bs=1024 count=3072 2>/dev/null; printf '\\n%s\\n' \"$1\"",
        END,
    ))
    .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.report.bytes > NATIVE_COMPLETION_LINE_LIMIT as u64);
    assert_eq!(output.stdout.report.truncated_lines, 1);
    assert_eq!(output.stdout.retained_document(), Some(END));
    assert_eq!(output.stdout.completion(), Err(CompletionError::Truncated));
    assert_eq!(last_report(&output.stdout), None);
    assert!(
        session_notice(last_report(&output.stdout), "mac", true)
            .unwrap_err()
            .unknown_effect
    );
}

#[test]
fn a_native_completion_larger_than_the_windows_budget_remains_valid() {
    let prefix = format!("{},\"padding\":\"", END.strip_suffix('}').unwrap());
    let output = run(&mut shell(
        "printf '%s' \"$1\"; i=0; while [ \"$i\" -lt 1024 ]; do printf '%0512d' 0; i=$((i+1)); done; printf '\"}\\n'",
        &prefix,
    )).unwrap();
    assert!(output.status.success());
    assert!(output.stdout.report.bytes > 512 * 1024);
    assert!(output.stdout.report.complete());
    assert_eq!(last_report(&output.stdout), Some(Report::Ended));
}

#[test]
fn actual_failed_child_cannot_commit_its_selection_or_success() {
    for (document, success) in [(SELECTED, false), (SELECTED, true), (END, false)] {
        let script = if success {
            "printf '%s\\n' \"$1\"; exit 0"
        } else {
            "printf '%s\\n' \"$1\"; exit 9"
        };
        let output = run(&mut shell(script, document)).unwrap();
        assert_eq!(output.status.success(), success);
        if document == SELECTED {
            assert_eq!(
                picker_report(&output, SELECTED_PORT, false).is_some(),
                success
            );
            assert_eq!(picker_report(&output, DEFAULT_SERVICE_PORT, false), None);
        }
        assert!(
            session_notice(last_report(&output.stdout), "mac", output.status.success())
                .unwrap_err()
                .unknown_effect
        );
    }
}

#[test]
fn a_later_invalid_terminal_record_never_reuses_a_live_child_selection() {
    for suffix in ["{truncated", "viewer crashed", "{}"] {
        let mut command = shell("printf '%s\\n%s\\n' \"$1\" \"$2\"", SELECTED);
        command.arg(suffix);
        let output = run(&mut command).unwrap();
        assert!(output.status.success());
        assert_eq!(picker_report(&output, SELECTED_PORT, false), None);
        assert!(
            session_notice(last_report(&output.stdout), "mac", output.status.success())
                .unwrap_err()
                .unknown_effect
        );
    }
}

#[test]
fn a_live_child_uncertain_error_stops_the_app_but_refusal_can_return_to_the_picker() {
    for (outcome, retryable) in [
        ("unknown_external_effect", false),
        ("refused", true),
        ("cancelled", true),
    ] {
        let document = serde_json::json!({
            "schema_version": 1,
            "outcome": outcome,
            "error": {"code": "viewer_ended", "next_action": "Inspect the original command."},
        })
        .to_string();
        let output = run(&mut shell("printf '%s\\n' \"$1\"; exit 9", &document)).unwrap();
        assert!(!output.status.success());
        let notice = session_notice(last_report(&output.stdout), "mac", output.status.success());
        assert_eq!(notice.is_ok(), retryable);
        if let Err(failure) = notice {
            let document = crate::output::failure(failure, true);
            let value: serde_json::Value = serde_json::from_str(&document).unwrap();
            assert_eq!(value["outcome"], "unknown_external_effect");
        }
    }
}

#[test]
fn read_failure_terminates_and_reaps_the_original_command() {
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }
    let child = shell("exec sleep 30", "").spawn().unwrap();
    let pid = child.id();
    let start = Instant::now();
    assert!(matches!(
        follow(child, Broken, None, DRAIN_TIMEOUT),
        Err(Error::Capture)
    ));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_reaped(pid);
}

#[test]
fn an_inherited_writer_cannot_outlive_the_original_child_drain_budget() {
    // The descendant holds stdout open until its private stdin closes. Ending
    // the test drops that pipe; no unrelated process is found or signalled.
    let mut child = shell(
        "exec 3<&0; (read -r stop <&3) & printf '%s\\n' \"$1\"; exit 0",
        END,
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()
    .unwrap();
    let stdout = child.stdout.take().unwrap();
    let flags = rustix::fs::fcntl_getfl(&stdout).unwrap();
    rustix::fs::fcntl_setfl(&stdout, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let pid = child.id();
    let start = Instant::now();
    let result = follow(child, stdout, None, Duration::from_millis(50));
    assert!(matches!(result, Err(Error::Capture)));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_reaped(pid);
}

pub(super) fn assert_reaped(pid: u32) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    assert_eq!(
        rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG).err(),
        Some(rustix::io::Errno::CHILD),
    );
}
