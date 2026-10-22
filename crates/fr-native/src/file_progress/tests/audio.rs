//! Actual native panel events drive the same preference handle used by audio.
//! Session liveness is a fixture; these tests do not claim live-tailnet playback.
use super::*;

struct AudioProbe {
    source: Transfer,
    stopped: AtomicBool,
}
impl Target for AudioProbe {
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    fn direction(&self) -> Direction {
        self.source.direction()
    }
    fn snapshot(&self) -> Snapshot {
        self.source.snapshot()
    }
    fn action(&self, action: Action) {
        self.source.action(action);
    }
}

#[test]
fn audio_commands_keep_mute_gain_and_unrelated_controls_independent() {
    let volume = AudioVolumeControl::from_parts(0.5, true);
    let source = Transfer::Audio(volume.clone());
    let unrelated = AudioVolumeControl::new();
    for _ in 0..20 {
        source.action(Action::Quieter);
    }
    assert_eq!(volume.snapshot(), (0.0, true));
    for _ in 0..20 {
        source.action(Action::Louder);
    }
    assert_eq!(volume.snapshot(), (1.0, true));
    for action in [Action::Pause, Action::Resume, Action::Cancel] {
        source.action(action);
    }
    assert_eq!(volume.snapshot(), (1.0, true));
    source.action(Action::Mute);
    assert_eq!(volume.snapshot(), (1.0, false));
    assert_eq!(unrelated.snapshot(), (1.0, false));
    let frame = Frame::snapshot(&source.snapshot());
    assert_eq!(frame.progress, 1000);
    assert!(frame.permits(Action::Mute));
    assert!(frame.permits(Action::Quieter));
    assert!(!frame.permits(Action::Louder));
    for action in [Action::Pause, Action::Resume, Action::Cancel] {
        assert!(!frame.permits(action));
    }
}

#[test]
fn actual_audio_buttons_change_shared_gain_without_stealing_remote_focus() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new_direction("audio") else {
        return;
    };
    let volume = AudioVolumeControl::from_parts(0.5, false);
    let probe = Arc::new(AudioProbe {
        source: Transfer::Audio(volume.clone()),
        stopped: AtomicBool::new(false),
    });
    let state = Arc::new(AtomicU8::new(0));
    let (copy, status, window) = (probe.clone(), state.clone(), peer.window);
    let task = thread::spawn(move || run(&display, window, &status, copy.as_ref()));
    wait(|| state.load(Ordering::Acquire) == 1 || task.is_finished());
    assert!(!task.is_finished(), "audio panel stopped before mapping");
    wait(|| peer.command("mapped") == "yes");
    assert_eq!(peer.command("title"), "FrankenRemote - remote audio");
    assert_eq!(peer.command("hints"), "nonfocus");
    assert_eq!(peer.command("placement"), "separate");
    assert_eq!(peer.command("focus"), "parent");
    for command in ["synthetic pause", "synthetic cancel", "release cancel"] {
        peer.command(command);
    }
    thread::sleep(REFRESH);
    assert_eq!(volume.snapshot(), (0.5, false));
    peer.command("click cancel"); // Third native button: mute, never file cancel.
    wait(|| volume.is_muted());
    peer.command("click pause"); // First native button: quieter.
    wait(|| volume.volume() < 0.45);
    assert!(volume.is_muted());
    peer.command("click resume"); // Second native button: louder.
    wait(|| volume.volume() > 0.45);
    assert!(volume.is_muted());
    peer.command("click cancel");
    wait(|| !volume.is_muted());
    assert_eq!(peer.command("focus"), "parent");
    assert!(!probe.stopped());
    let preference = volume.snapshot();
    peer.command("close");
    wait(|| task.is_finished());
    assert_eq!(task.join().unwrap(), StopReason::Hidden);
    assert_eq!(volume.snapshot(), preference);
    assert!(!probe.stopped());
    assert_eq!(peer.command("exists"), "no");
}

#[test]
fn a_fullscreen_view_keeps_initial_preferences_without_a_covering_panel() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new_layout("audio", true) else {
        return;
    };
    let volume = AudioVolumeControl::from_parts(0.25, true);
    let probe = AudioProbe {
        source: Transfer::Audio(volume.clone()),
        stopped: AtomicBool::new(false),
    };
    assert_eq!(
        run(&display, peer.window, &AtomicU8::new(0), &probe),
        StopReason::NativeFailure
    );
    assert_eq!(volume.snapshot(), (0.25, true));
    assert_eq!(peer.command("exists"), "no");
    assert_eq!(peer.command("focus"), "parent");
    assert!(!probe.stopped());
}
