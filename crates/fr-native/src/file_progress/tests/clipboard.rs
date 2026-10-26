//! Real XCB mapping, drawing and input with explicit clipboard/session fixtures.
//! These qualify the local off surface, not authenticated transfer or OS text.
use super::*;
use frd::native_clipboard::{Cleanup, Phase, Status as ClipboardStatus};

struct ClipboardProbe {
    enabled: AtomicBool,
    session_stopped: AtomicBool,
    requests: Mutex<Vec<Action>>,
}
impl ClipboardProbe {
    fn new() -> Self {
        Self {
            enabled: AtomicBool::new(true),
            session_stopped: AtomicBool::new(false),
            requests: Mutex::new(Vec::new()),
        }
    }
}
impl Target for ClipboardProbe {
    fn stopped(&self) -> bool {
        self.session_stopped.load(Ordering::Acquire)
    }
    fn direction(&self) -> Direction {
        Direction::Clipboard
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot::Clipboard(ClipboardStatus {
            phase: Phase::Running,
            enabled: self.enabled.load(Ordering::Acquire),
            cleanup: Cleanup::NotStarted,
            reason: None,
        })
    }
    fn action(&self, action: Action) {
        assert_eq!(action, Action::TurnOff);
        self.requests.lock().unwrap().push(action);
        self.enabled.store(false, Ordering::Release);
    }
    fn panel_stopped(&self) {
        self.enabled.store(false, Ordering::Release);
    }
}

fn start_clipboard(
    display: CString,
    window: crate::viewer_input::Window,
) -> (Arc<ClipboardProbe>, Arc<AtomicU8>, JoinHandle<StopReason>) {
    let probe = Arc::new(ClipboardProbe::new());
    let state = Arc::new(AtomicU8::new(0));
    let (copy, status) = (probe.clone(), state.clone());
    let task = thread::spawn(move || run(&display, window, &status, copy.as_ref()));
    wait(|| state.load(Ordering::Acquire) == 1 || task.is_finished());
    assert!(!task.is_finished(), "clipboard panel did not become usable");
    (probe, state, task)
}

#[test]
fn mapped_off_control_preserves_focus_and_requests_only_original_clipboard_stop() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new_direction("clipboard") else {
        return;
    };
    let (probe, state, task) = start_clipboard(display, peer.window);
    let replacement = ClipboardProbe::new();
    assert_eq!(peer.command("mapped"), "yes");
    assert_eq!(peer.command("title"), "FrankenRemote - shared clipboard");
    assert_eq!(peer.command("hints"), "nonfocus");
    assert_eq!(peer.command("placement"), "separate");
    assert_eq!(peer.command("focus"), "parent");
    for command in [
        "click pause",
        "click resume",
        "synthetic off",
        "release off",
        "press off",
        "release outside",
    ] {
        peer.command(command);
    }
    thread::sleep(REFRESH);
    assert!(probe.enabled.load(Ordering::Acquire));
    assert!(probe.requests.lock().unwrap().is_empty());
    peer.command("click off");
    wait(|| !probe.enabled.load(Ordering::Acquire));
    assert_eq!(*probe.requests.lock().unwrap(), [Action::TurnOff]);
    assert!(!probe.stopped());
    assert!(replacement.enabled.load(Ordering::Acquire));
    assert_eq!(peer.command("focus"), "parent");
    assert_eq!(peer.command("mapped"), "yes");
    peer.command("click off");
    thread::sleep(REFRESH);
    assert_eq!(*probe.requests.lock().unwrap(), [Action::TurnOff]);
    peer.command("close");
    wait(|| task.is_finished());
    assert_eq!(task.join().unwrap(), StopReason::Hidden);
    assert_eq!(
        status(state.load(Ordering::Acquire)),
        Status::Stopped(StopReason::Hidden)
    );
    assert!(!probe.stopped());
    assert!(replacement.enabled.load(Ordering::Acquire));
}

#[test]
fn losing_privacy_surface_requests_clipboard_stop_without_requesting_desktop_stop() {
    let _serial = SERIAL.lock().unwrap();
    for fault in [
        "close",
        "unmap",
        "resize",
        "parent_unmap",
        "cover",
        "cover_partial",
        "panel_overlap",
        "panel_offscreen",
        "parent_offscreen",
    ] {
        let Some((mut peer, display)) = Peer::new_direction("clipboard") else {
            return;
        };
        let (probe, _, task) = start_clipboard(display, peer.window);
        peer.command(fault);
        wait(|| task.is_finished());
        assert_eq!(task.join().unwrap(), StopReason::Hidden);
        assert!(
            !probe.enabled.load(Ordering::Acquire),
            "{fault} retained clipboard"
        );
        assert!(!probe.stopped(), "{fault} stopped the desktop fixture");
        assert!(probe.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn unavailable_panel_fences_clipboard_before_any_mapping_claim() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new_layout("clipboard", true) else {
        return;
    };
    let probe = ClipboardProbe::new();
    let state = AtomicU8::new(0);
    assert_eq!(
        run(&display, peer.window, &state, &probe),
        StopReason::NativeFailure
    );
    assert!(!probe.enabled.load(Ordering::Acquire));
    assert!(!probe.stopped());
    assert_ne!(status(state.load(Ordering::Acquire)), Status::Running);
    assert_eq!(peer.command("exists"), "no");
}

#[test]
fn clipboard_frames_never_offer_transfer_or_audio_actions_or_private_text() {
    for phase in [
        Phase::WaitingForControl,
        Phase::Negotiating,
        Phase::Running,
        Phase::Retired,
        Phase::Closed,
    ] {
        for enabled in [false, true] {
            let frame = Frame::snapshot(&Snapshot::Clipboard(ClipboardStatus {
                phase,
                enabled,
                cleanup: Cleanup::Pending,
                reason: Some(frd::clipboard_quic::Error::Closed),
            }));
            let active = enabled && !matches!(phase, Phase::Retired | Phase::Closed);
            assert_eq!(frame.permits(Action::TurnOff), active);
            assert_eq!(frame.actions, if active { 64 } else { 0 });
            assert_eq!(frame.progress, -1);
            let label = |row: &[u8; model::COLUMNS]| {
                let end = row.iter().position(|b| *b == 0).unwrap();
                String::from_utf8(row[..end].to_vec()).unwrap()
            };
            assert_eq!(
                label(&frame.lines[2]),
                "Turn off ends text sharing for this connection."
            );
            assert_eq!(
                label(&frame.lines[3]),
                "If a transfer cannot stop independently, the connection closes."
            );
            for row in frame.lines {
                let end = row.iter().position(|b| *b == 0).unwrap();
                assert!(row[..end].iter().all(|b| (32..=126).contains(b)));
                assert!(row[end..].iter().all(|b| *b == 0));
            }
        }
    }
}

#[test]
fn compact_privacy_surface_uses_reserved_strip_without_covering_the_viewer() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new_window("clipboard", "reserved") else {
        return;
    };
    let (probe, _, task) = start_clipboard(display, peer.window);
    assert_eq!(peer.command("size"), "640 96");
    assert_eq!(peer.command("placement"), "separate");
    assert_eq!(peer.command("focus"), "parent");
    assert!(probe.enabled.load(Ordering::Acquire));
    peer.command("click off");
    wait(|| !probe.enabled.load(Ordering::Acquire));
    assert_eq!(*probe.requests.lock().unwrap(), [Action::TurnOff]);
    assert!(!probe.stopped());
    peer.command("close");
    wait(|| task.is_finished());
    assert_eq!(task.join().unwrap(), StopReason::Hidden);
}
