//! Actual C drawing/events plus the production Rust loop. Session authority and
//! file outcomes are explicit fixtures, not authenticated transfer qualification.
mod audio;
mod clipboard;
mod receive;
use super::*;
use frd::native_files::{SendPhase, SendSnapshot, SendStatus};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Mutex, atomic::AtomicBool},
};
static SERIAL: Mutex<()> = Mutex::new(());
pub(super) fn snapshot() -> SendSnapshot {
    SendSnapshot {
        status: SendStatus {
            phase: SendPhase::WaitingForControl,
            absence: None,
            report: None,
        },
        stage: None,
        current: None,
        cancel_requested: false,
        pause_requested: false,
        paused: None,
        cleanup: None,
    }
}
struct Probe {
    sample: Mutex<SendSnapshot>,
    commands: Mutex<Vec<Action>>,
    stopped: AtomicBool,
    stall: AtomicBool,
    stalled: AtomicBool,
}
impl Probe {
    fn new() -> Self {
        Self {
            sample: Mutex::new(snapshot()),
            commands: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            stall: AtomicBool::new(false),
            stalled: AtomicBool::new(false),
        }
    }
    fn sample(&self) -> SendSnapshot {
        *self.sample.lock().unwrap()
    }
}
impl Target for Probe {
    fn stopped(&self) -> bool {
        if self.stall.load(Ordering::Acquire) {
            self.stalled.store(true, Ordering::Release);
            while self.stall.load(Ordering::Acquire) {
                thread::sleep(TURN);
            }
        }
        self.stopped.load(Ordering::Acquire)
    }
    fn direction(&self) -> Direction {
        Direction::Upload
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot::Upload(Box::new(self.sample()))
    }
    fn action(&self, action: Action) {
        let mut commands = self.commands.lock().unwrap();
        assert!(commands.len() < 16);
        commands.push(action);
        let mut sample = self.sample.lock().unwrap();
        match action {
            Action::Pause => sample.pause_requested = true,
            Action::Resume => sample.pause_requested = false,
            Action::Cancel => sample.cancel_requested = true,
            Action::Quieter | Action::Louder | Action::Mute | Action::TurnOff => {
                panic!("unrelated action in file panel");
            }
        }
    }
}
struct Peer {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    window: crate::viewer_input::Window,
}
impl Peer {
    fn new() -> Option<(Self, CString)> {
        Self::new_direction("upload")
    }
    fn new_direction(direction: &str) -> Option<(Self, CString)> {
        Self::new_layout(direction, false)
    }
    fn new_layout(direction: &str, full_screen: bool) -> Option<(Self, CString)> {
        Self::new_window(
            direction,
            if full_screen {
                "full-screen"
            } else {
                "ordinary"
            },
        )
    }
    fn new_window(direction: &str, layout: &str) -> Option<(Self, CString)> {
        let Ok(display) = std::env::var("DISPLAY") else {
            assert!(
                std::env::var_os("FR_NATIVE_UPLOAD_PANEL_REQUIRED").is_none(),
                "required upload panel tests need DISPLAY"
            );
            eprintln!("BLOCKED: no X11 display; upload panel is not qualified");
            return None;
        };
        let mut child = Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/upload_progress/peer.py"
            ))
            .arg(&display)
            .arg(direction)
            .arg(layout)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let mut fields = line
            .split_ascii_whitespace()
            .map(|f| f.parse::<u32>().unwrap());
        let window = crate::viewer_input::Window {
            id: fields.next().unwrap(),
            width: fields.next().unwrap(),
            height: fields.next().unwrap(),
        };
        assert!(fields.next().is_none());
        Some((
            Self {
                child,
                input,
                output,
                window,
            },
            CString::new(display).unwrap(),
        ))
    }
    fn command(&mut self, command: &str) -> String {
        writeln!(self.input, "{command}").unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        line.trim().to_owned()
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = writeln!(self.input, "quit");
        let _ = self.input.flush();
        let _ = self.child.wait();
    }
}
pub(super) fn wait(mut check: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(3);
    while !check() {
        assert!(Instant::now() < until, "upload UI did not progress");
        thread::sleep(TURN);
    }
}
fn start(
    display: CString,
    window: crate::viewer_input::Window,
) -> (Arc<Probe>, Arc<AtomicU8>, JoinHandle<StopReason>) {
    let probe = Arc::new(Probe::new());
    let state = Arc::new(AtomicU8::new(0));
    let (copy, status) = (probe.clone(), state.clone());
    let task = thread::spawn(move || run(&display, window, &status, copy.as_ref()));
    wait(|| state.load(Ordering::Acquire) == 1 || task.is_finished());
    assert!(
        !task.is_finished(),
        "native panel initialization failed: {:?}",
        task.join().unwrap()
    );
    (probe, state, task)
}
fn finish(probe: &Probe, state: &AtomicU8, task: JoinHandle<StopReason>) {
    probe.stopped.store(true, Ordering::Release);
    assert_eq!(task.join().unwrap(), StopReason::SessionEnded);
    assert_eq!(
        status(state.load(Ordering::Acquire)),
        Status::Stopped(StopReason::SessionEnded)
    );
}
#[test]
fn actual_buttons_request_file_only_pause_resume_cancel_without_taking_focus() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, state, task) = start(display, peer.window);
    wait(|| peer.command("mapped") == "yes");
    assert_eq!(peer.command("hints"), "nonfocus");
    assert_eq!(peer.command("placement"), "separate");
    assert_eq!(peer.command("focus"), "parent");
    for (index, command) in ["click pause", "click resume", "click cancel"]
        .into_iter()
        .enumerate()
    {
        assert_eq!(peer.command(command), "ok");
        wait(|| probe.commands.lock().unwrap().len() == index + 1 || task.is_finished());
        assert!(!task.is_finished());
        assert_eq!(peer.command("focus"), "parent");
        thread::sleep(REFRESH);
    }
    for command in ["click pause", "click resume", "click cancel"] {
        peer.command(command);
    }
    thread::sleep(REFRESH);
    assert_eq!(
        *probe.commands.lock().unwrap(),
        [Action::Pause, Action::Resume, Action::Cancel]
    );
    finish(&probe, &state, task);
    assert_eq!(peer.command("exists"), "no");
}
#[test]
fn disabled_synthetic_dragged_and_unmatched_releases_cannot_request_work() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, state, task) = start(display, peer.window);
    for command in [
        "click resume",
        "synthetic",
        "release pause",
        "press pause",
        "release cancel",
        "press outside",
        "release pause",
    ] {
        peer.command(command);
    }
    thread::sleep(REFRESH);
    assert_eq!(*probe.commands.lock().unwrap(), []);
    peer.command("click pause");
    wait(|| probe.commands.lock().unwrap().len() == 1);
    finish(&probe, &state, task);
}
#[test]
fn real_pixels_reflect_queued_progress_not_a_completion_claim() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, state, task) = start(display, peer.window);
    assert_eq!(peer.command("bar"), "0");
    probe.sample.lock().unwrap().current = Some(fr_files::sender::Progress {
        id: 1,
        queued_bytes: 5,
        total_bytes: Some(10),
    });
    wait(|| peer.command("bar") == "301");
    probe
        .sample
        .lock()
        .unwrap()
        .current
        .as_mut()
        .unwrap()
        .queued_bytes = 10;
    wait(|| peer.command("bar") == "602");
    assert_eq!(probe.sample().status.phase, SendPhase::WaitingForControl);
    assert!(probe.sample().status.report.is_none());
    finish(&probe, &state, task);
}
#[test]
fn whole_batch_close_and_parent_loss_withhold_commands_without_cancelling_files() {
    let _serial = SERIAL.lock().unwrap();
    for fault in ["close", "unmap", "resize", "parent_unmap"] {
        let Some((mut peer, display)) = Peer::new() else {
            return;
        };
        let (probe, state, task) = start(display, peer.window);
        probe.stall.store(true, Ordering::Release);
        wait(|| probe.stalled.load(Ordering::Acquire));
        peer.command("click pause");
        peer.command(fault);
        probe.stall.store(false, Ordering::Release);
        wait(|| task.is_finished());
        assert_eq!(task.join().unwrap(), StopReason::Hidden);
        assert_eq!(*probe.commands.lock().unwrap(), []);
        assert!(!probe.sample().cancel_requested);
        assert_eq!(
            status(state.load(Ordering::Acquire)),
            Status::Stopped(StopReason::Hidden)
        );
    }
}
#[test]
fn stale_clicks_and_event_flood_never_restart_or_cancel_the_selection() {
    let _serial = SERIAL.lock().unwrap();
    for flood in [false, true] {
        let Some((mut peer, display)) = Peer::new() else {
            return;
        };
        let (probe, state, task) = start(display, peer.window);
        probe.stall.store(true, Ordering::Release);
        wait(|| probe.stalled.load(Ordering::Acquire));
        peer.command("click pause");
        if flood {
            peer.command("flood");
        } else {
            thread::sleep(MAX_CLICK_AGE + REFRESH);
        }
        probe.stall.store(false, Ordering::Release);
        thread::sleep(REFRESH);
        assert_eq!(*probe.commands.lock().unwrap(), []);
        if flood {
            assert_eq!(task.join().unwrap(), StopReason::EventFlood);
        } else {
            assert!(!task.is_finished());
            peer.command("click pause");
            wait(|| probe.commands.lock().unwrap().len() == 1);
            finish(&probe, &state, task);
        }
    }
}
#[test]
fn a_terminal_receipt_racing_a_click_keeps_every_command_disabled() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, state, task) = start(display, peer.window);
    probe.stall.store(true, Ordering::Release);
    wait(|| probe.stalled.load(Ordering::Acquire));
    peer.command("click pause");
    probe.sample.lock().unwrap().status.phase = SendPhase::Finished;
    probe.stall.store(false, Ordering::Release);
    thread::sleep(REFRESH);
    assert_eq!(*probe.commands.lock().unwrap(), []);
    finish(&probe, &state, task);
}
#[test]
fn stopped_original_session_refuses_native_open_and_never_applies_a_command() {
    let state = AtomicU8::new(0);
    let probe = Probe::new();
    probe.stopped.store(true, Ordering::Release);
    assert_eq!(
        run(
            &CString::new(":65535").unwrap(),
            crate::viewer_input::Window {
                id: 1,
                width: 320,
                height: 240
            },
            &state,
            &probe
        ),
        StopReason::SessionEnded
    );
    assert_eq!(*probe.commands.lock().unwrap(), []);
}
