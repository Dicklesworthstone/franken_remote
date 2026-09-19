//! Actual X11 capture, with explicitly synthetic user events and Target authority.
use super::*;

#[test]
fn emergency_chord_fences_original_target_without_exporting_escape() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, task) = start(display, peer.window, capabilities());
    for (code, count) in [(37, 1), (64, 2), (50, 3)] {
        peer.command(&format!("key {code} 1"));
        wait(|| probe.events.lock().unwrap().len() >= count || task.is_finished());
        assert!(!task.is_finished());
    }
    peer.command("key 9 1");
    wait(|| task.is_finished());
    let result = task.join().unwrap();
    for code in [9, 50, 64, 37] {
        peer.command(&format!("key {code} 0"));
    }
    assert_eq!(result, Err(StopReason::LocalEscape));
    assert!(probe.stopped.load(Ordering::Acquire));
    assert!(
        probe
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|(event, _)| !matches!(event,
        Event::Key { key, .. } if key.usage() == 0x29))
    );
}

#[test]
fn emergency_chord_withholds_the_complete_batch_with_or_without_keyboard_permission() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for caps in [
        capabilities(),
        Capabilities::default().with(Capability::Absolute),
    ] {
        let Some((mut peer, display)) = Peer::new() else {
            return;
        };
        let (probe, task) = start(display, peer.window, caps);
        // Pause native dispatch so this entire chord occupies one bounded batch.
        probe.pause.store(true, Ordering::Release);
        wait(|| probe.paused.load(Ordering::Acquire));
        for code in [37, 64, 50, 9] {
            peer.command(&format!("key {code} 1"));
        }
        probe.pause.store(false, Ordering::Release);
        wait(|| task.is_finished());
        let result = task.join().unwrap();
        for code in [9, 50, 64, 37] {
            peer.command(&format!("key {code} 0"));
        }
        assert_eq!(result, Err(StopReason::LocalEscape));
        assert!(probe.stopped.load(Ordering::Acquire));
        assert!(probe.events.lock().unwrap().is_empty());
    }
}
