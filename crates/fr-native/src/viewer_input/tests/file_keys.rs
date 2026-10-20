//! Actual X11 event source and production input loop. `XTest` supplies local
//! test gestures; original file/transport authority is an explicit Target seam.
use super::super::file_keys::Command as FileCommand;
use super::*;
fn release(peer: &mut Peer) {
    for code in [74, 75, 76, 9, 37, 64, 50] {
        peer.command(&format!("key {code} 0"));
    }
}
fn chord(peer: &mut Peer, code: u32) {
    for code in [37, 64, 50, code] {
        peer.command(&format!("key {code} 1"));
    }
}
fn commands(probe: &Probe) -> Vec<FileCommand> {
    probe.file_commands.lock().unwrap().clone()
}
fn function_events(probe: &Probe) -> usize {
    probe.events.lock().unwrap().iter().filter(|(event,_)| {
        matches!(event, Event::Key{key,..} if (0x41..=0x43).contains(&key.usage()))
    }).count()
}
#[test]
fn actual_x11_shortcuts_dispatch_file_only_commands_and_keep_input_running() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, task) = start(display, peer.window, capabilities());
    probe.file_controls.store(true, Ordering::Release);
    for (index, code) in [74, 75, 76].into_iter().enumerate() {
        chord(&mut peer, code);
        wait(|| commands(&probe).len() > index || task.is_finished());
        release(&mut peer);
    }
    // A transfer cancellation request is not an input/session cancellation.
    peer.command("key 38 1");
    peer.command("key 38 0");
    wait(|| {
        probe
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| matches!(e,Event::Key{key,..} if key.usage()==4))
            .count()
            == 2
            || task.is_finished()
    });
    assert!(!task.is_finished());
    probe.stopped.store(true, Ordering::Release);
    assert_eq!(task.join().unwrap(), Err(StopReason::Closed));
    assert_eq!(
        commands(&probe),
        [FileCommand::Pause, FileCommand::Resume, FileCommand::Cancel]
    );
    assert_eq!(function_events(&probe), 0);
    assert_eq!(probe.file_keymap.load(Ordering::Acquire), 1);
    assert_eq!(
        probe
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| matches!(
                e,
                Event::Key {
                    transition: KeyTransition::Release,
                    ..
                }
            ))
            .count(),
        10
    );
}
#[test]
fn unconfigured_shortcuts_still_deliver_function_keys_to_the_remote_target() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, task) = start(display, peer.window, capabilities());
    for code in [74, 75, 76] {
        chord(&mut peer, code);
        release(&mut peer);
    }
    wait(|| function_events(&probe) == 6 || task.is_finished());
    assert!(!task.is_finished());
    probe.stopped.store(true, Ordering::Release);
    assert_eq!(task.join().unwrap(), Err(StopReason::Closed));
    assert_eq!(function_events(&probe), 6);
    assert_eq!(commands(&probe), []);
}
#[test]
fn file_shortcuts_do_not_require_remote_keyboard_permission_or_adopt_held_keys() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let caps = Capabilities::default().with(Capability::Absolute);
    let (probe, task) = start(display, peer.window, caps);
    probe.file_controls.store(true, Ordering::Release);
    chord(&mut peer, 74);
    wait(|| commands(&probe).len() == 1 || task.is_finished());
    // Only locally held keys must still drive the SAME release-only sampler.
    wait(|| {
        probe
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(e, _)| matches!(e, Event::HeldState(_)))
            || task.is_finished()
    });
    release(&mut peer);
    peer.command("motion 70 80");
    wait(|| {
        probe
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(e, _)| matches!(e, Event::Pointer(_)))
            || task.is_finished()
    });
    assert!(!task.is_finished());
    probe.stopped.store(true, Ordering::Release);
    assert_eq!(task.join().unwrap(), Err(StopReason::Closed));
    assert_eq!(commands(&probe), [FileCommand::Pause]);
    assert!(
        probe
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|(e, _)| !matches!(e, Event::Key { .. }))
    );
}
#[test]
fn a_remote_function_key_keeps_its_release_when_modifiers_arrive_later() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, task) = start(display, peer.window, capabilities());
    probe.file_controls.store(true, Ordering::Release);
    peer.command("key 74 1");
    wait(|| function_events(&probe) == 1 || task.is_finished());
    for code in [37, 64, 50] {
        peer.command(&format!("key {code} 1"));
    }
    release(&mut peer);
    wait(|| function_events(&probe) == 2 || task.is_finished());
    assert!(!task.is_finished());
    probe.stopped.store(true, Ordering::Release);
    assert_eq!(task.join().unwrap(), Err(StopReason::Closed));
    assert_eq!(function_events(&probe), 2);
    assert_eq!(commands(&probe), []);
}
#[test]
fn invalid_native_batch_cannot_resume_or_cancel_any_transfer() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for fault in ["synthetic", "focus", "resize", "hierarchy"] {
        let Some((mut peer, display)) = Peer::new() else {
            return;
        };
        let (probe, task) = start(display, peer.window, capabilities());
        probe.file_controls.store(true, Ordering::Release);
        probe.pause.store(true, Ordering::Release);
        wait(|| probe.paused.load(Ordering::Acquire));
        chord(&mut peer, 75);
        peer.command(fault);
        probe.pause.store(false, Ordering::Release);
        wait(|| task.is_finished());
        release(&mut peer);
        assert!(task.join().unwrap().is_err());
        assert_eq!(commands(&probe), []);
        assert!(probe.events.lock().unwrap().is_empty());
    }
}
#[test]
fn emergency_escape_withholds_file_commands_in_the_same_native_batch() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, task) = start(display, peer.window, capabilities());
    probe.file_controls.store(true, Ordering::Release);
    probe.pause.store(true, Ordering::Release);
    wait(|| probe.paused.load(Ordering::Acquire));
    chord(&mut peer, 75);
    peer.command("key 9 1");
    probe.pause.store(false, Ordering::Release);
    wait(|| task.is_finished());
    release(&mut peer);
    assert_eq!(task.join().unwrap(), Err(StopReason::LocalEscape));
    assert_eq!(commands(&probe), []);
    assert!(probe.events.lock().unwrap().is_empty());
}
#[test]
fn aged_native_shortcuts_are_not_retimed_into_fresh_transfer_requests() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, task) = start(display, peer.window, capabilities());
    probe.file_controls.store(true, Ordering::Release);
    probe.pause.store(true, Ordering::Release);
    wait(|| probe.paused.load(Ordering::Acquire));
    chord(&mut peer, 75);
    thread::sleep(Duration::from_millis(150));
    probe.pause.store(false, Ordering::Release);
    wait(|| task.is_finished());
    release(&mut peer);
    assert_eq!(task.join().unwrap(), Err(StopReason::Expired));
    assert_eq!(commands(&probe), []);
}
