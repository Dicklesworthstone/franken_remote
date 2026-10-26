//! Concurrent production Rust/C panels with explicit transfer/session fixtures.
//! Native placement and command ownership are tested, not authenticated ATP.
use super::*;

struct Placed<T> {
    target: Arc<T>,
    placement: FilePlacement,
}
impl<T: Target> Target for Placed<T> {
    fn stopped(&self) -> bool {
        self.target.stopped()
    }
    fn direction(&self) -> Direction {
        self.target.direction()
    }
    fn file_placement(&self) -> FilePlacement {
        self.placement
    }
    fn snapshot(&self) -> Snapshot {
        self.target.snapshot()
    }
    fn action(&self, action: Action) {
        self.target.action(action);
    }
}
fn placed<T: Target + Send + Sync + 'static>(
    display: CString,
    window: crate::viewer_input::Window,
    target: Arc<T>,
    secondary: bool,
) -> (Arc<AtomicU8>, JoinHandle<StopReason>) {
    let state = Arc::new(AtomicU8::new(0));
    let copy = state.clone();
    let task = thread::spawn(move || {
        run(
            &display,
            window,
            &copy,
            &Placed {
                target,
                placement: FilePlacement {
                    secondary,
                    clipboard: true,
                    first_reserves_clipboard: secondary,
                },
            },
        )
    });
    wait(|| state.load(Ordering::Acquire) == 1 || task.is_finished());
    assert!(
        !task.is_finished(),
        "concurrent native panel refused its reserved slot"
    );
    (state, task)
}

#[test]
fn both_file_directions_keep_separate_controls_and_leave_clipboard_visible() {
    let _serial = SERIAL.lock().unwrap();
    for upload_first in [true, false] {
        let Some((mut peer, display)) = Peer::new_direction("clipboard") else {
            return;
        };
        let (clipboard, clipboard_state, clipboard_task) =
            clipboard::start_clipboard(display.clone(), peer.window);
        let upload = Arc::new(Probe::new());
        let download = Arc::new(receive::DownloadProbe::new());
        let ((upload_state, upload_task), (download_state, download_task)) = if upload_first {
            let send = placed(display.clone(), peer.window, upload.clone(), false);
            let receive = placed(display.clone(), peer.window, download.clone(), true);
            (send, receive)
        } else {
            let receive = placed(display.clone(), peer.window, download.clone(), false);
            let send = placed(display.clone(), peer.window, upload.clone(), true);
            (send, receive)
        };
        for direction in ["upload", "download"] {
            assert_eq!(peer.command(&format!("direction {direction}")), "ok");
            assert_eq!(peer.command("mapped"), "yes");
            assert_eq!(peer.command("size"), "640 310");
            assert_eq!(peer.command("placement"), "separate");
            assert_eq!(peer.command("separate_from clipboard"), "separate");
            assert_eq!(peer.command("hints"), "nonfocus");
            assert_eq!(peer.command("focus"), "parent");
        }
        assert_eq!(peer.command("separate_from upload"), "separate");
        thread::sleep(REFRESH);
        assert!(clipboard.enabled.load(Ordering::Acquire));
        assert_eq!(
            status(clipboard_state.load(Ordering::Acquire)),
            Status::Running
        );
        peer.command("click cancel");
        wait(|| download.commands.lock().unwrap().len() == 1);
        assert_eq!(*download.commands.lock().unwrap(), [Action::Cancel]);
        assert!(upload.commands.lock().unwrap().is_empty());
        assert!(clipboard.enabled.load(Ordering::Acquire));
        peer.command("direction upload");
        for (index, command) in ["click pause", "click resume", "click cancel"]
            .iter()
            .enumerate()
        {
            peer.command(command);
            wait(|| upload.commands.lock().unwrap().len() == index + 1);
        }
        assert_eq!(
            *upload.commands.lock().unwrap(),
            [Action::Pause, Action::Resume, Action::Cancel]
        );
        assert_eq!(*download.commands.lock().unwrap(), [Action::Cancel]);
        peer.command("direction download");
        peer.command("close");
        wait(|| download_task.is_finished());
        assert_eq!(download_task.join().unwrap(), StopReason::Hidden);
        assert_eq!(
            status(download_state.load(Ordering::Acquire)),
            Status::Stopped(StopReason::Hidden)
        );
        assert_eq!(
            status(upload_state.load(Ordering::Acquire)),
            Status::Running
        );
        assert!(clipboard.enabled.load(Ordering::Acquire));
        peer.command("direction clipboard");
        peer.command("click off");
        wait(|| !clipboard.enabled.load(Ordering::Acquire));
        assert_eq!(
            status(upload_state.load(Ordering::Acquire)),
            Status::Running
        );
        stop(&upload_state, StopReason::OwnerStopped);
        stop(&clipboard_state, StopReason::OwnerStopped);
        assert_eq!(upload_task.join().unwrap(), StopReason::OwnerStopped);
        assert_eq!(clipboard_task.join().unwrap(), StopReason::OwnerStopped);
    }
}
