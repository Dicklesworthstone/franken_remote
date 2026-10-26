//! The production bounded swap and native-window loop, with explicit local
//! transfer/session fixtures. These are not authenticated ATP transfer proofs.
use super::*;

#[derive(Clone)]
struct Batch {
    id: u8,
    finished: Arc<AtomicBool>,
}
impl Batch {
    fn new(id: u8, finished: bool) -> Self {
        Self {
            id,
            finished: Arc::new(AtomicBool::new(finished)),
        }
    }
}
impl UploadStatus for Batch {
    fn finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

#[test]
fn upload_replacement_waits_for_terminal_work_and_owns_one_pending_slot() {
    let state = AtomicU8::new(1);
    let original = Batch::new(1, false);
    let next = Batch::new(2, false);
    let later = Batch::new(3, false);
    let mut swap = UploadSwap {
        current: original.clone(),
        pending: None,
    };
    assert!(!swap.queue(&state, next.clone()));
    assert!(swap.pending.is_none());
    assert_eq!(status(state.load(Ordering::Acquire)), Status::Running);
    original.finished.store(true, Ordering::Release);
    assert!(swap.queue(&state, next.clone()));
    assert_eq!(
        swap.current.id, original.id,
        "old native events keep their owner"
    );
    assert_eq!(status(state.load(Ordering::Acquire)), Status::Opening);
    assert!(!swap.queue(&state, later.clone()));
    swap.ready(&state);
    assert_eq!(
        status(state.load(Ordering::Acquire)),
        Status::Opening,
        "mapping evidence from the old window cannot ready its successor"
    );
    // Production calls take only after run_window has destroyed Native.
    assert_eq!(swap.take(&state).unwrap().id, next.id);
    assert!(swap.pending.is_none());
    assert_eq!(swap.current.id, next.id);
    assert!(!swap.queue(&state, later.clone()));
    swap.ready(&state);
    assert_eq!(status(state.load(Ordering::Acquire)), Status::Running);
    next.finished.store(true, Ordering::Release);
    assert!(swap.queue(&state, later));
}

#[test]
fn hidden_failed_and_ended_panels_cannot_accept_or_install_successors() {
    for reason in [
        StopReason::Hidden,
        StopReason::NativeFailure,
        StopReason::SessionEnded,
        StopReason::OwnerStopped,
        StopReason::EventFlood,
    ] {
        for already_queued in [false, true] {
            let state = AtomicU8::new(1);
            let mut swap = UploadSwap {
                current: Batch::new(1, true),
                pending: None,
            };
            if already_queued {
                assert!(swap.queue(&state, Batch::new(2, false)));
            }
            stop(&state, reason);
            assert!(!swap.queue(&state, Batch::new(3, false)));
            assert!(matches!(swap.take(&state), Err(StopReason::OwnerStopped)));
            swap.ready(&state);
            assert_eq!(
                status(state.load(Ordering::Acquire)),
                Status::Stopped(reason)
            );
            assert_eq!(swap.current.id, 1);
            assert_eq!(swap.pending.is_some(), already_queued);
        }
    }
}

struct ReplacementTarget {
    probe: Arc<Probe>,
    pending: Arc<AtomicBool>,
}
impl Target for ReplacementTarget {
    fn stopped(&self) -> bool {
        self.probe.stopped()
    }
    fn direction(&self) -> Direction {
        Direction::Upload
    }
    fn snapshot(&self) -> Snapshot {
        self.probe.snapshot()
    }
    fn action(&self, action: Action) {
        self.probe.action(action);
    }
    fn replacement_pending(&self) -> Result<bool, StopReason> {
        Ok(self.pending.load(Ordering::Acquire))
    }
}

#[test]
fn replacement_destroys_the_old_window_before_new_clicks_can_select_new_work() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let original = Arc::new(Probe::new());
    let next = Arc::new(Probe::new());
    let pending = Arc::new(AtomicBool::new(false));
    let state = Arc::new(AtomicU8::new(0));
    let (cleaned, old_cleanup) = std::sync::mpsc::sync_channel(1);
    let (resume, next_window) = std::sync::mpsc::sync_channel(1);
    let (first, second, requested, shared) = (
        original.clone(),
        next.clone(),
        pending.clone(),
        state.clone(),
    );
    let window = peer.window;
    let task = thread::spawn(move || {
        assert_eq!(
            run_window(
                &display,
                window,
                &shared,
                &ReplacementTarget {
                    probe: first,
                    pending: requested,
                }
            ),
            RunEnd::Replace
        );
        cleaned.send(()).unwrap();
        next_window.recv().unwrap();
        run(&display, window, &shared, second.as_ref())
    });
    wait(|| state.load(Ordering::Acquire) == 1 || task.is_finished());
    assert!(!task.is_finished());
    original.stall.store(true, Ordering::Release);
    wait(|| original.stalled.load(Ordering::Acquire));
    peer.command("click pause");
    peer.command("press cancel");
    original.sample.lock().unwrap().status.phase = SendPhase::Finished;
    pending.store(true, Ordering::Release);
    original.stall.store(false, Ordering::Release);
    old_cleanup.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(peer.command("exists"), "no");
    assert_eq!(status(state.load(Ordering::Acquire)), Status::Opening);
    assert!(original.commands.lock().unwrap().is_empty());
    resume.send(()).unwrap();
    wait(|| state.load(Ordering::Acquire) == 1 || task.is_finished());
    assert!(!task.is_finished());
    assert_eq!(peer.command("placement"), "separate");
    assert_eq!(peer.command("focus"), "parent");
    peer.command("release cancel");
    thread::sleep(REFRESH);
    assert!(
        next.commands.lock().unwrap().is_empty(),
        "old presses cannot arm new controls"
    );
    peer.command("click pause");
    wait(|| next.commands.lock().unwrap().len() == 1);
    assert_eq!(*next.commands.lock().unwrap(), [Action::Pause]);
    assert!(original.commands.lock().unwrap().is_empty());
    finish(&next, &state, task);
}

#[test]
fn queued_native_close_and_parent_loss_win_over_pending_replacement() {
    let _serial = SERIAL.lock().unwrap();
    for fault in ["close", "unmap", "parent_unmap"] {
        let Some((mut peer, display)) = Peer::new() else {
            return;
        };
        let original = Arc::new(Probe::new());
        let pending = Arc::new(AtomicBool::new(false));
        let state = Arc::new(AtomicU8::new(0));
        let (probe, requested, shared) = (original.clone(), pending.clone(), state.clone());
        let window = peer.window;
        let task = thread::spawn(move || {
            run_window(
                &display,
                window,
                &shared,
                &ReplacementTarget {
                    probe,
                    pending: requested,
                },
            )
        });
        wait(|| state.load(Ordering::Acquire) == 1 || task.is_finished());
        assert!(!task.is_finished());
        original.stall.store(true, Ordering::Release);
        wait(|| original.stalled.load(Ordering::Acquire));
        peer.command("click pause");
        peer.command(fault);
        original.sample.lock().unwrap().status.phase = SendPhase::Finished;
        pending.store(true, Ordering::Release);
        original.stall.store(false, Ordering::Release);
        wait(|| task.is_finished());
        assert_eq!(task.join().unwrap(), RunEnd::Stopped(StopReason::Hidden));
        assert_eq!(
            status(state.load(Ordering::Acquire)),
            Status::Stopped(StopReason::Hidden)
        );
        assert!(original.commands.lock().unwrap().is_empty());
        assert_eq!(peer.command("exists"), "no");
    }
}
