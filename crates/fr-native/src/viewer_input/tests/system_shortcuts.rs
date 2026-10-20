//! Real X11 grabs and delivery, with the existing explicit Target fixture.
//! The independent peer owns passive root shortcuts like a local window manager;
//! these tests do not qualify a compositor, physical user, or remote authority.
use super::*;
use KeyTransition::{Press, Release};

struct Capture {
    probe: Arc<Probe>,
    task: Option<JoinHandle<Result<(), StopReason>>>,
}
impl Capture {
    fn start(display: CString, window: Window, captured: bool) -> Self {
        let (probe, task) = start_using(display, window, capabilities(), captured);
        Self {
            probe,
            task: Some(task),
        }
    }
    fn finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }
    fn join(&mut self) -> Result<(), StopReason> {
        wait(|| self.finished());
        self.task.take().unwrap().join().unwrap()
    }
    fn stop(&self) {
        self.probe.stopped.store(true, Ordering::Release);
        self.probe.pause.store(false, Ordering::Release);
    }
    fn pause(&self) {
        self.probe.pause.store(true, Ordering::Release);
        wait(|| self.probe.paused.load(Ordering::Acquire));
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        // A failed assertion must not leave a grab or a paused native owner on
        // the shared test server. The original Target is fenced before joining.
        self.stop();
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}
fn keys(probe: &Probe) -> Vec<(u16, KeyTransition)> {
    probe
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(event, _)| match event {
            Event::Key { key, transition } => Some((key.usage(), *transition)),
            _ => None,
        })
        .collect()
}
fn codes(peer: &mut Peer) -> [u32; 3] {
    peer.query("shortcut_codes")
        .split_whitespace()
        .map(|code| code.parse().unwrap())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap()
}
fn refused(
    display: &CString,
    window: Window,
    caps: Capabilities,
) -> (Result<(), StopReason>, Arc<Probe>) {
    let probe = Arc::new(Probe::default());
    let (_, layout) = viewport();
    let result = run_with_system_shortcuts(
        display,
        window,
        &layout,
        &mut Recording {
            start: Instant::now(),
            probe: probe.clone(),
            caps,
        },
    );
    assert!(!probe.ready.load(Ordering::Acquire));
    assert!(probe.events.lock().unwrap().is_empty());
    (result, probe)
}

#[test]
fn explicit_capture_delivers_alt_tab_and_super_past_local_passive_grabs() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let [_, _, super_code] = codes(&mut peer);
    peer.command("passive_shortcuts");
    let mut capture = Capture::start(display, peer.window, true);
    assert_eq!(peer.query("probe_grab"), "1"); // AlreadyGrabbed by capture.
    let title = peer.query("title");
    assert!(title.contains("Remote system shortcuts"));
    assert!(title.contains("Ctrl+Alt+Shift+Escape"));
    peer.command("shortcut alt_tab");
    peer.command("shortcut super");
    wait(|| keys(&capture.probe).len() >= 6 || capture.finished());
    assert!(!capture.finished());
    assert_eq!(
        keys(&capture.probe),
        [
            (0xe2, Press),
            (0x2b, Press),
            (0x2b, Release),
            (0xe2, Release),
            (0xe3, Press),
            (0xe3, Release),
        ]
    );
    assert_eq!(peer.query("local_keys"), "");
    capture.stop();
    assert_eq!(capture.join(), Err(StopReason::Closed));
    assert_eq!(peer.query("probe_grab"), "0");
    assert_eq!(peer.query("title"), "FrankenRemote");
    // The fixture's original local binding survives capture cleanup.
    peer.command("shortcut super");
    assert_eq!(
        peer.query("local_keys"),
        format!("{super_code}:2 {super_code}:3")
    );
}

#[test]
fn default_capture_leaves_system_shortcuts_with_the_local_passive_owner() {
    let _serial = SERIAL.lock().unwrap();
    for shortcut in ["alt_tab", "super"] {
        let Some((mut peer, display)) = Peer::new() else {
            return;
        };
        let [_, tab, super_code] = codes(&mut peer);
        let (code, usage) = if shortcut == "alt_tab" {
            (tab, 0x2b)
        } else {
            (super_code, 0xe3)
        };
        peer.command("passive_shortcuts");
        let mut capture = Capture::start(display, peer.window, false);
        peer.command(&format!("shortcut {shortcut}"));
        assert_eq!(capture.join(), Err(StopReason::FocusLost));
        assert_eq!(peer.query("local_keys"), format!("{code}:2 {code}:3"));
        assert!(keys(&capture.probe).iter().all(|(key, _)| *key != usage));
        assert_eq!(peer.query("probe_grab"), "0");
    }
}

#[test]
fn a_competing_active_grab_refuses_without_stealing_or_silent_fallback() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    assert_eq!(peer.query("grab"), "0");
    let (result, _) = refused(&display, peer.window, capabilities());
    assert_eq!(result, Err(StopReason::SystemShortcutsUnavailable));
    // The third connection still sees the original competitor's active grab.
    assert_eq!(peer.query("probe_grab"), "1");
    peer.command("ungrab");
    assert_eq!(peer.query("probe_grab"), "0");
    let mut capture = Capture::start(display, peer.window, true);
    assert_eq!(peer.query("probe_grab"), "1");
    capture.stop();
    assert_eq!(capture.join(), Err(StopReason::Closed));
    assert_eq!(peer.query("probe_grab"), "0");
}

#[test]
fn missing_keyboard_permission_never_grabs_or_disables_local_shortcuts() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let [_, _, super_code] = codes(&mut peer);
    peer.command("passive_shortcuts");
    let (result, _) = refused(
        &display,
        peer.window,
        Capabilities::default().with(Capability::Absolute),
    );
    assert_eq!(result, Err(StopReason::SystemShortcutsUnavailable));
    assert_eq!(peer.query("probe_grab"), "0");
    peer.command("shortcut super");
    assert_eq!(
        peer.query("local_keys"),
        format!("{super_code}:2 {super_code}:3")
    );
}

#[test]
fn preexisting_keys_and_buttons_are_refused_without_adopting_a_held_gesture() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let [alt, _, super_code] = codes(&mut peer);
    for held in [
        format!("key {alt} 1"),
        format!("key {super_code} 1"),
        "button 1 1".into(),
    ] {
        peer.command(&held);
        let (result, _) = refused(&display, peer.window, capabilities());
        peer.command("release");
        assert_eq!(result, Err(StopReason::InitialHeld));
        assert_eq!(peer.query("probe_grab"), "0");
    }
}

#[test]
fn real_focus_loss_withholds_the_batch_releases_capture_and_requires_a_fresh_owner() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    peer.command("passive_shortcuts");
    let mut capture = Capture::start(display.clone(), peer.window, true);
    capture.pause();
    peer.command("key 38 1");
    // SetInputFocus produces NotifyWhileGrabbed, not an activation notification.
    peer.command("focus");
    peer.command("key 38 0");
    capture.probe.pause.store(false, Ordering::Release);
    assert_eq!(capture.join(), Err(StopReason::FocusLost));
    assert!(capture.probe.stopped.load(Ordering::Acquire));
    assert!(capture.probe.events.lock().unwrap().is_empty());
    assert_eq!(peer.query("probe_grab"), "0");
    peer.command("restore");
    peer.command("shortcut super");
    assert_ne!(peer.query("local_keys"), "");
    assert!(capture.probe.events.lock().unwrap().is_empty());
    // A new explicit fixture owner can capture; restoring focus never resumed
    // the stopped one. Production additionally requires a new original grant.
    let mut next = Capture::start(display, peer.window, true);
    peer.command("shortcut super");
    wait(|| keys(&next.probe).len() >= 2 || next.finished());
    assert_eq!(
        keys(&next.probe),
        [(0xe3, KeyTransition::Press), (0xe3, KeyTransition::Release)]
    );
    assert_eq!(peer.query("local_keys"), "");
    next.stop();
    assert_eq!(next.join(), Err(StopReason::Closed));
    assert_eq!(peer.query("probe_grab"), "0");
}

#[test]
fn emergency_escape_stays_local_and_releases_capture_across_or_within_batches() {
    let _serial = SERIAL.lock().unwrap();
    for one_batch in [false, true] {
        let Some((mut peer, display)) = Peer::new() else {
            return;
        };
        let mut capture = Capture::start(display, peer.window, true);
        if one_batch {
            capture.pause();
        }
        for (code, count) in [(37, 1), (64, 2), (50, 3)] {
            peer.command(&format!("key {code} 1"));
            if !one_batch {
                wait(|| keys(&capture.probe).len() >= count || capture.finished());
                assert!(!capture.finished());
            }
        }
        peer.command("key 9 1");
        capture.probe.pause.store(false, Ordering::Release);
        let result = capture.join();
        peer.command("release");
        assert_eq!(result, Err(StopReason::LocalEscape));
        assert!(capture.probe.stopped.load(Ordering::Acquire));
        assert!(keys(&capture.probe).iter().all(|(usage, _)| *usage != 0x29));
        if one_batch {
            assert!(capture.probe.events.lock().unwrap().is_empty());
        }
        assert_eq!(peer.query("probe_grab"), "0");
    }
}

#[test]
fn keymap_window_and_synthetic_fences_release_the_owned_native_grab() {
    let _serial = SERIAL.lock().unwrap();
    for (command, reason) in [
        ("remap", StopReason::KeymapChanged),
        ("resize", StopReason::WindowChanged),
        ("synthetic", StopReason::SyntheticInput),
        ("synthetic_focus", StopReason::FocusLost),
        ("unmap", StopReason::WindowChanged),
    ] {
        let Some((mut peer, display)) = Peer::new() else {
            return;
        };
        let mut capture = Capture::start(display, peer.window, true);
        peer.command(command);
        let result = capture.join();
        if command == "unmap" {
            assert!(matches!(
                result,
                Err(StopReason::WindowChanged | StopReason::FocusLost)
            ));
            peer.command("map");
        } else {
            assert_eq!(result, Err(reason));
        }
        assert!(capture.probe.stopped.load(Ordering::Acquire));
        assert!(capture.probe.events.lock().unwrap().is_empty());
        assert_eq!(peer.query("probe_grab"), "0");
    }
}

#[test]
fn stopping_the_original_target_releases_capture_even_while_a_key_is_held() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let [alt, _, _] = codes(&mut peer);
    let mut capture = Capture::start(display, peer.window, true);
    peer.command(&format!("key {alt} 1"));
    wait(|| keys(&capture.probe).len() == 1 || capture.finished());
    assert!(!capture.finished());
    capture.stop();
    assert_eq!(capture.join(), Err(StopReason::Closed));
    assert_eq!(peer.query("probe_grab"), "0");
    peer.command("release");
    // Native cleanup releases only its grab. The original remote input agent,
    // exercised separately, owns release of already-submitted remote modifiers.
    assert_eq!(keys(&capture.probe), [(0xe2, KeyTransition::Press)]);
}
