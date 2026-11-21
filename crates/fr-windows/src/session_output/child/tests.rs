use super::*;
use std::io::Cursor;

#[test]
fn reader_panic_reports_through_the_bounded_completion_channel() {
    let (sender, receiver) = mpsc::sync_channel(2);
    let reader =
        spawn_reader(Stream::Stdout, sender, || panic!("injected reader failure")).unwrap();
    let event = receiver.recv().unwrap();
    assert!(matches!(event, Event::Panicked(Stream::Stdout)));
    assert!(
        reader.join().is_ok(),
        "the reader must report its failure, not escape"
    );
}

#[test]
fn failed_status_read_keeps_bounded_first_guidance_and_failure() {
    struct Failure(Cursor<&'static [u8]>);
    impl Read for Failure {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            match self.0.read(out)? {
                0 => Err(io::ErrorKind::BrokenPipe.into()),
                count => Ok(count),
            }
        }
    }
    let (capture, log) = capture_status(
        Failure(Cursor::new(b"fr-windows: original refusal\n")),
        None,
    );
    assert_eq!(
        capture.first_line.as_deref(),
        Some(b"fr-windows: original refusal".as_slice())
    );
    assert_eq!(capture.report.read_error, Some(io::ErrorKind::BrokenPipe));
    assert!(!capture.report.complete());
    assert!(log.is_none());
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::{
        fs::File,
        process::{Command, Stdio},
    };

    const REPORT: &str = r#"{"status":"ended","end":"ok","drawn":12}"#;

    fn child(script: &str) -> Child {
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn sleeper() -> Child {
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    #[test]
    fn both_real_pipes_drain_beyond_pipe_capacity_without_losing_completion() {
        let process = child(
            r#"i=0
while [ "$i" -lt 20000 ]; do
    printf 'viewer diagnostic before completion\n'
    printf 'fr-windows: reconnecting\n' >&2
    i=$((i+1))
done
printf '%s\n' '{"status":"ended","end":"ok","drawn":12}'
"#,
        );
        let output = follow_child(process, None);
        assert_eq!(output.failure, None);
        assert_eq!(output.cleanup_error, None);
        assert!(output.complete());
        let stdout = output.stdout.unwrap();
        assert_eq!(stdout.completion().unwrap(), REPORT);
        assert_eq!(stdout.report.lines, 20001);
        let stderr = output.stderr.unwrap();
        assert_eq!(stderr.report.lines, 20000);
        assert_eq!(
            stderr.first_line.as_deref(),
            Some(b"fr-windows: reconnecting".as_slice())
        );
    }

    #[test]
    fn nonzero_child_exit_never_becomes_success_from_a_prior_json_report() {
        let output = follow_child(
            child(r#"printf '%s\n' '{"status":"ended","end":"ok","drawn":12}'; exit 7"#),
            None,
        );
        assert_eq!(output.status.unwrap().code(), Some(7));
        assert_eq!(output.failure, None);
        assert!(!output.complete());
        assert_eq!(output.stdout.unwrap().completion().unwrap(), REPORT);
    }

    #[test]
    fn oversized_unterminated_stderr_is_drained_and_reported() {
        let process = child(
            r#"i=0
while [ "$i" -lt 10000 ]; do
    printf '0123456789abcdef0123456789abcdef' >&2
    i=$((i+1))
done
printf '%s' '{"status":"ended","end":"ok","drawn":12}'
"#,
        );
        let output = follow_child(process, None);
        assert!(output.status.unwrap().success());
        assert_eq!(output.failure, None);
        assert!(!output.complete());
        assert_eq!(output.stdout.unwrap().completion().unwrap(), REPORT);
        let stderr = output.stderr.unwrap();
        assert_eq!(stderr.report.bytes, 320_000);
        assert_eq!(stderr.report.lines, 1);
        assert_eq!(stderr.report.truncated_lines, 1);
        assert_eq!(
            stderr.first_line.unwrap().len(),
            super::super::super::STATUS_LINE_LIMIT
        );
    }

    #[test]
    fn missing_pipe_stops_and_reaps_the_original_child() {
        let mut process = sleeper();
        drop(process.stderr.take());
        let output = follow_child(process, None);
        assert_eq!(output.failure, Some(FollowError::MissingPipe));
        assert_eq!(output.cleanup_error, None);
        assert!(!output.status.unwrap().success());
        assert!(!output.complete());
    }

    /// Inject a real pipe read failure without relying on OS timing or unsafe
    /// descriptor manipulation. The second pipe still belongs to the child.
    struct FailedPipe<R>(R);
    impl<R: Read> Read for FailedPipe<R> {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }

    #[test]
    fn pipe_read_failure_stops_and_reaps_while_the_other_reader_is_waiting() {
        let mut process = sleeper();
        let stdout = process.stdout.take().unwrap();
        let stderr = process.stderr.take().unwrap();
        let output = follow_pipes(process, FailedPipe(stdout), stderr, None);
        assert_eq!(
            output.failure,
            Some(FollowError::Read(Stream::Stdout, io::ErrorKind::BrokenPipe))
        );
        assert_eq!(output.cleanup_error, None);
        assert!(!output.status.unwrap().success());
        assert!(!output.complete());
        assert_eq!(
            output.stdout.unwrap().report.read_error,
            Some(io::ErrorKind::BrokenPipe)
        );
        assert!(output.stderr.unwrap().report.complete());
    }

    struct PanickingPipe<R>(R);
    impl<R: Read> Read for PanickingPipe<R> {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            panic!("injected original pipe failure")
        }
    }

    #[test]
    fn pipe_reader_panic_stops_and_reaps_without_waiting_for_normal_child_exit() {
        let mut process = sleeper();
        let stdout = process.stdout.take().unwrap();
        let stderr = process.stderr.take().unwrap();
        let output = follow_pipes(process, PanickingPipe(stdout), stderr, None);
        assert_eq!(
            output.failure,
            Some(FollowError::ReaderPanicked(Stream::Stdout))
        );
        assert_eq!(output.cleanup_error, None);
        assert!(!output.status.unwrap().success());
        assert!(!output.complete());
        assert!(output.stdout.is_none());
        assert!(output.stderr.unwrap().report.complete());
    }

    #[test]
    fn log_write_failure_cannot_stop_real_pipe_draining_or_claim_complete_capture() {
        let log = CappedLog {
            file: File::open("/dev/null").unwrap(),
        };
        let output = follow_child(
            child(
                r#"printf 'fr-windows: first status\nfr-windows: second status\n' >&2
printf '%s\n' '{"status":"ended","end":"ok","drawn":12}'
"#,
            ),
            Some(log),
        );
        assert!(output.status.unwrap().success());
        assert_eq!(output.failure, None);
        assert!(!output.complete());
        let stderr = output.stderr.unwrap();
        assert!(stderr.report.complete());
        assert_eq!(stderr.report.lines, 2);
        assert!(stderr.log_error.is_some());
        assert!(output.log.is_none());
        assert_eq!(output.stdout.unwrap().completion().unwrap(), REPORT);
    }
}
