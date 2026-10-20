//! Real X11 drawing/events on the shared production boundary. Receive metadata
//! and session-stop signals remain explicit fixtures, not authenticated disk I/O.
use super::*;
use fr_files::{receive::Publication, session, wire::ResultReceipt, worker::Completion};
use frd::native_files::{ReceivePhase, ReceiveStatus};

struct DownloadProbe {
    sample: Mutex<ReceiveStatus>,
    commands: Mutex<Vec<Action>>,
    stopped: AtomicBool,
    stall: AtomicBool,
    stalled: AtomicBool,
}
impl DownloadProbe {
    fn new() -> Self {
        Self {
            sample: Mutex::new(ReceiveStatus {
                phase: ReceivePhase::Receiving,
                absence: None,
                progress: None,
                results: vec![ResultReceipt {
                    id: 1,
                    outcome: Ok(Completion::Published(session::Receipt {
                        id: 1,
                        bytes: 0,
                        publication: Publication::Durable,
                    })),
                }],
                results_truncated: false,
                cancel_requested: false,
                cleanup: None,
            }),
            commands: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            stall: AtomicBool::new(false),
            stalled: AtomicBool::new(false),
        }
    }
}
impl Target for DownloadProbe {
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
        Direction::Download
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot::Download(self.sample.lock().unwrap().clone())
    }
    fn action(&self, action: Action) {
        assert_eq!(
            action,
            Action::Cancel,
            "downloads never acquire sender pacing controls"
        );
        let mut commands = self.commands.lock().unwrap();
        assert_eq!(
            commands.len(),
            0,
            "the original cancellation is requested at most once"
        );
        commands.push(action);
        self.sample.lock().unwrap().cancel_requested = true;
    }
}
fn start_download(
    display: CString,
    window: crate::viewer_input::Window,
) -> (Arc<DownloadProbe>, Arc<AtomicU8>, JoinHandle<StopReason>) {
    let probe = Arc::new(DownloadProbe::new());
    let state = Arc::new(AtomicU8::new(0));
    let (copy, status) = (probe.clone(), state.clone());
    let task = thread::spawn(move || run(&display, window, &status, copy.as_ref()));
    wait(|| state.load(Ordering::Acquire) == 1 || task.is_finished());
    assert!(
        !task.is_finished(),
        "native download panel failed: {:?}",
        task.join().unwrap()
    );
    (probe, state, task)
}
fn finish_download(probe: &DownloadProbe, state: &AtomicU8, task: JoinHandle<StopReason>) {
    probe.stopped.store(true, Ordering::Release);
    assert_eq!(task.join().unwrap(), StopReason::SessionEnded);
    assert_eq!(
        status(state.load(Ordering::Acquire)),
        Status::Stopped(StopReason::SessionEnded)
    );
}

#[test]
fn actual_download_button_only_requests_file_cancellation_and_preserves_results() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new_direction("download") else {
        return;
    };
    let (probe, state, task) = start_download(display, peer.window);
    let receipts = probe.sample.lock().unwrap().results.clone();
    wait(|| peer.command("mapped") == "yes");
    assert_eq!(peer.command("title"), "FrankenRemote - download progress");
    assert_eq!(peer.command("hints"), "nonfocus");
    assert_eq!(peer.command("placement"), "separate");
    assert_eq!(peer.command("focus"), "parent");
    for command in [
        "click pause",
        "click resume",
        "synthetic cancel",
        "release cancel",
    ] {
        assert_eq!(peer.command(command), "ok");
    }
    thread::sleep(REFRESH);
    assert_eq!(*probe.commands.lock().unwrap(), [] as [Action; 0]);
    peer.command("click cancel");
    wait(|| probe.sample.lock().unwrap().cancel_requested);
    assert_eq!(peer.command("focus"), "parent");
    assert!(!probe.stopped.load(Ordering::Acquire));
    thread::sleep(REFRESH);
    peer.command("click cancel");
    thread::sleep(REFRESH);
    assert_eq!(*probe.commands.lock().unwrap(), [Action::Cancel]);
    assert_eq!(probe.sample.lock().unwrap().phase, ReceivePhase::Receiving);
    assert_eq!(probe.sample.lock().unwrap().results, receipts);
    assert!(probe.sample.lock().unwrap().cleanup.is_none());
    finish_download(&probe, &state, task);
    assert_eq!(peer.command("exists"), "no");
}

#[test]
fn actual_download_pixels_track_staging_without_fabricating_another_publication() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new_direction("download") else {
        return;
    };
    let (probe, state, task) = start_download(display, peer.window);
    let receipts = probe.sample.lock().unwrap().results.clone();
    assert_eq!(peer.command("bar"), "0");
    probe.sample.lock().unwrap().progress = Some(session::Progress {
        id: 2,
        staged_bytes: 5,
        total_bytes: 10,
    });
    wait(|| peer.command("bar") == "301");
    probe
        .sample
        .lock()
        .unwrap()
        .progress
        .as_mut()
        .unwrap()
        .staged_bytes = 10;
    wait(|| peer.command("bar") == "602");
    assert_eq!(probe.sample.lock().unwrap().results, receipts);
    assert_eq!(probe.sample.lock().unwrap().phase, ReceivePhase::Receiving);
    finish_download(&probe, &state, task);
}

#[test]
fn hiding_or_losing_the_parent_keeps_download_receipts_and_never_cancels() {
    let _serial = SERIAL.lock().unwrap();
    for fault in ["close", "parent_unmap"] {
        let Some((mut peer, display)) = Peer::new_direction("download") else {
            return;
        };
        let (probe, _, task) = start_download(display, peer.window);
        let receipts = probe.sample.lock().unwrap().results.clone();
        probe.stall.store(true, Ordering::Release);
        wait(|| probe.stalled.load(Ordering::Acquire));
        peer.command("click cancel");
        peer.command(fault);
        probe.stall.store(false, Ordering::Release);
        wait(|| task.is_finished());
        assert_eq!(task.join().unwrap(), StopReason::Hidden);
        assert_eq!(*probe.commands.lock().unwrap(), [] as [Action; 0]);
        assert!(!probe.sample.lock().unwrap().cancel_requested);
        assert!(!probe.stopped.load(Ordering::Acquire));
        assert_eq!(probe.sample.lock().unwrap().results, receipts);
    }
}

#[test]
fn terminal_receipts_stale_clicks_and_floods_cannot_cancel_or_restart_downloads() {
    let _serial = SERIAL.lock().unwrap();
    for fault in ["terminal", "stale", "flood"] {
        let Some((mut peer, display)) = Peer::new_direction("download") else {
            return;
        };
        let (probe, state, task) = start_download(display, peer.window);
        let receipts = probe.sample.lock().unwrap().results.clone();
        probe.stall.store(true, Ordering::Release);
        wait(|| probe.stalled.load(Ordering::Acquire));
        peer.command("click cancel");
        match fault {
            "terminal" => probe.sample.lock().unwrap().phase = ReceivePhase::Ended,
            "stale" => thread::sleep(MAX_CLICK_AGE + REFRESH),
            "flood" => {
                peer.command("flood");
            }
            _ => unreachable!(),
        }
        probe.stall.store(false, Ordering::Release);
        thread::sleep(REFRESH);
        assert_eq!(*probe.commands.lock().unwrap(), [] as [Action; 0]);
        assert!(!probe.sample.lock().unwrap().cancel_requested);
        assert_eq!(probe.sample.lock().unwrap().results, receipts);
        if fault == "flood" {
            assert_eq!(task.join().unwrap(), StopReason::EventFlood);
        } else {
            assert!(!task.is_finished());
            finish_download(&probe, &state, task);
        }
    }
}

#[test]
fn no_room_for_download_progress_never_covers_the_viewer_or_cancels_files() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new_layout("download", true) else {
        return;
    };
    let probe = DownloadProbe::new();
    let receipts = probe.sample.lock().unwrap().results.clone();
    let state = AtomicU8::new(0);
    assert_eq!(
        run(&display, peer.window, &state, &probe),
        StopReason::NativeFailure
    );
    assert_eq!(peer.command("exists"), "no");
    assert_eq!(peer.command("focus"), "parent");
    assert_eq!(*probe.commands.lock().unwrap(), [] as [Action; 0]);
    assert!(!probe.sample.lock().unwrap().cancel_requested);
    assert!(!probe.stopped.load(Ordering::Acquire));
    assert_eq!(probe.sample.lock().unwrap().results, receipts);
}

#[test]
fn later_parent_or_panel_movement_hides_progress_and_withholds_queued_cancel() {
    let _serial = SERIAL.lock().unwrap();
    for movement in ["parent_overlap", "panel_overlap", "panel_offscreen"] {
        let Some((mut peer, display)) = Peer::new_direction("download") else {
            return;
        };
        let (probe, _, task) = start_download(display, peer.window);
        let receipts = probe.sample.lock().unwrap().results.clone();
        wait(|| peer.command("mapped") == "yes");
        assert_eq!(peer.command("placement"), "separate");
        probe.stall.store(true, Ordering::Release);
        wait(|| probe.stalled.load(Ordering::Acquire));
        peer.command("click cancel");
        peer.command(movement);
        probe.stall.store(false, Ordering::Release);
        wait(|| task.is_finished());
        assert!(matches!(
            task.join().unwrap(),
            StopReason::Hidden | StopReason::NativeFailure
        ));
        assert_eq!(peer.command("exists"), "no");
        assert_eq!(*probe.commands.lock().unwrap(), [] as [Action; 0]);
        assert!(!probe.sample.lock().unwrap().cancel_requested);
        assert!(!probe.stopped.load(Ordering::Acquire));
        assert_eq!(probe.sample.lock().unwrap().results, receipts);
    }
}

#[test]
fn unchanged_status_still_detects_an_ancestor_moving_over_the_panel() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new_window("download", "framed") else {
        return;
    };
    let (probe, _, task) = start_download(display, peer.window);
    let original = probe.sample.lock().unwrap().clone();
    wait(|| peer.command("mapped") == "yes");
    assert_eq!(peer.command("placement"), "separate");
    // Moving only the fixture's outer frame generates no ConfigureNotify on
    // the original nested client under Xvfb. No action or changed status forces
    // a draw; the periodic native geometry validation must hide the panel.
    peer.command("parent_overlap");
    wait(|| task.is_finished());
    assert_eq!(task.join().unwrap(), StopReason::Hidden);
    assert_eq!(peer.command("exists"), "no");
    assert_eq!(*probe.sample.lock().unwrap(), original);
    assert_eq!(*probe.commands.lock().unwrap(), [] as [Action; 0]);
    assert!(!probe.stopped.load(Ordering::Acquire));
}
