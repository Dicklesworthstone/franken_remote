//! Actual production Rust capture loop and C/X11 owner. Recording is an
//! explicit source/authority fixture; this does not qualify ATP or a portal UI.
use super::*;
use crate::viewer_input::file_choice::{Gate, Phase, now};

struct DialogTarget {
    recording: Recording,
    gate: Arc<Gate>,
}
impl Target for DialogTarget {
    fn clock(&self) -> Result<ClientInstant, StopReason> {
        self.recording.clock()
    }
    fn capabilities(&self) -> Capabilities {
        self.recording.capabilities()
    }
    fn push(&mut self, event: Event, sampled: ClientInstant) -> Result<(), StopReason> {
        self.recording.push(event, sampled)
    }
    fn ready(&self) {
        self.gate.enable();
        self.recording.ready();
    }
    fn file_choice_gate(&self) -> Option<Arc<Gate>> {
        Some(Arc::clone(&self.gate))
    }
    fn input_idle(&self) -> bool {
        // Recording consumes admitted events immediately. The real Source
        // implementation separately requires its original queue to be empty.
        true
    }
    fn window_resized(&self, width: u32, height: u32) {
        self.recording.window_resized(width, height);
    }
    fn stop(&self, reason: StopReason) {
        self.gate.stop();
        self.recording.stop(reason);
    }
}

fn start_dialogs(
    display: CString,
    window: Window,
    shortcuts: bool,
) -> (Arc<Probe>, Arc<Gate>, JoinHandle<Result<(), StopReason>>) {
    let probe = Arc::new(Probe::default());
    let gate = Arc::new(Gate::default());
    let (recorded, original) = (Arc::clone(&probe), Arc::clone(&gate));
    let (_, layout) = viewport();
    let task = thread::spawn(move || {
        let mut target = DialogTarget {
            recording: Recording {
                start: Instant::now(),
                probe: recorded,
                caps: capabilities(),
            },
            gate: original,
        };
        run_focused_mode(
            &display,
            window,
            &layout,
            &mut target,
            FOCUS_WAIT,
            shortcuts,
        )
    });
    wait(|| probe.ready.load(Ordering::Acquire) || task.is_finished());
    assert!(
        !task.is_finished(),
        "original input owner did not initialize"
    );
    (probe, gate, task)
}

fn await_phase(gate: &Gate, serial: u64, phase: Phase, task: &JoinHandle<Result<(), StopReason>>) {
    wait(|| gate.phase(serial, now().unwrap()) == Ok(phase) || task.is_finished());
    assert!(
        !task.is_finished(),
        "original input owner ended before {phase:?}"
    );
    assert_eq!(gate.phase(serial, now().unwrap()), Ok(phase));
}

fn pause(gate: &Gate, task: &JoinHandle<Result<(), StopReason>>) -> u64 {
    let serial = gate.request(now().unwrap()).unwrap();
    await_phase(gate, serial, Phase::Paused, task);
    serial
}

fn resume(gate: &Gate, serial: u64, task: &JoinHandle<Result<(), StopReason>>) {
    gate.resume(serial, now().unwrap()).unwrap();
    await_phase(gate, serial, Phase::Resumed, task);
    assert!(gate.finish(serial));
}

fn keys(probe: &Probe) -> usize {
    probe
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|(event, _)| matches!(event, Event::Key { .. }))
        .count()
}

#[test]
fn original_capture_pauses_for_local_dialog_input_and_resumes_twice_without_replay() {
    let _serial = SERIAL.lock().unwrap();
    for shortcuts in [false, true] {
        let Some((mut peer, display)) = Peer::new() else {
            return;
        };
        let (probe, gate, task) = start_dialogs(display, peer.window, shortcuts);
        peer.command("key 38 1");
        peer.command("key 38 0");
        wait(|| keys(&probe) == 2 || task.is_finished());
        assert!(!task.is_finished());
        let first = pause(&gate, &task);
        let before = probe.events.lock().unwrap().len();
        // Even explicit system-shortcut mode releases only its own grab while
        // paused, so the local dialog can receive ordinary keyboard input.
        assert_eq!(peer.query("probe_grab"), "0");
        peer.command("focus");
        for command in [
            "key 38 1",
            "key 38 0",
            "motion 40 50",
            "button 1 1",
            "button 1 0",
        ] {
            peer.command(command);
        }
        thread::sleep(Duration::from_millis(80));
        assert!(!task.is_finished());
        assert_eq!(probe.events.lock().unwrap().len(), before);
        // A positive local close request cannot resume an unfocused producer.
        gate.resume(first, now().unwrap()).unwrap();
        thread::sleep(Duration::from_millis(40));
        assert_eq!(gate.phase(first, now().unwrap()), Ok(Phase::Returning));
        assert!(!task.is_finished());
        peer.command("restore");
        await_phase(&gate, first, Phase::Resumed, &task);
        assert!(gate.finish(first));
        assert_eq!(probe.events.lock().unwrap().len(), before);
        peer.command("key 38 1");
        peer.command("key 38 0");
        wait(|| keys(&probe) == 4 || task.is_finished());
        assert!(!task.is_finished());
        assert!(!probe.stopped.load(Ordering::Acquire));
        let second = pause(&gate, &task);
        assert_ne!(first, second);
        resume(&gate, second, &task); // A declined dialog needs no new source.
        peer.command("key 38 1");
        peer.command("key 38 0");
        wait(|| keys(&probe) == 6 || task.is_finished());
        assert!(!task.is_finished());
        probe.stopped.store(true, Ordering::Release);
        assert_eq!(task.join().unwrap(), Err(StopReason::Closed));
        assert_eq!(gate.request(now().unwrap()), Err(()));
    }
}

#[test]
fn held_key_refuses_before_suspension_and_keeps_its_original_release() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, gate, task) = start_dialogs(display, peer.window, false);
    peer.command("key 38 1");
    wait(|| keys(&probe) == 1 || task.is_finished());
    assert!(!task.is_finished());
    let refused = gate.request(now().unwrap()).unwrap();
    await_phase(&gate, refused, Phase::Refused, &task);
    assert!(gate.finish(refused));
    assert!(!probe.stopped.load(Ordering::Acquire));
    peer.command("key 38 0");
    wait(|| keys(&probe) == 2 || task.is_finished());
    assert!(!task.is_finished());
    assert!(probe.events.lock().unwrap().iter().any(|(event, _)| {
        matches!(
            event,
            Event::Key {
                transition: KeyTransition::Release,
                ..
            }
        )
    }));
    let accepted = pause(&gate, &task);
    resume(&gate, accepted, &task);
    probe.stopped.store(true, Ordering::Release);
    assert_eq!(task.join().unwrap(), Err(StopReason::Closed));
}

#[test]
fn source_stop_window_loss_and_synthetic_focus_stay_terminal_during_the_dialog() {
    let _serial = SERIAL.lock().unwrap();
    for (command, expected) in [
        (None, StopReason::Closed),
        (Some("unmap"), StopReason::WindowChanged),
        (Some("synthetic_focus"), StopReason::SyntheticInput),
    ] {
        let Some((mut peer, display)) = Peer::new() else {
            return;
        };
        let (probe, gate, task) = start_dialogs(display, peer.window, false);
        let original = pause(&gate, &task);
        let before = probe.events.lock().unwrap().len();
        if let Some(command) = command {
            peer.command(command);
        } else {
            probe.stopped.store(true, Ordering::Release);
        }
        wait(|| task.is_finished());
        assert_eq!(task.join().unwrap(), Err(expected));
        assert_eq!(probe.events.lock().unwrap().len(), before);
        assert!(probe.stopped.load(Ordering::Acquire));
        assert_eq!(gate.resume(original, now().unwrap()), Err(()));
        assert!(!gate.finish(original));
        gate.enable();
        assert_eq!(gate.request(now().unwrap()), Err(()));
    }
}
