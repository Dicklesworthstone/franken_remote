//! Actual X11 event capture. Target/initial permission is an explicit fixture;
//! production uses the original Source, tested separately in frd. No compositor,
//! live tailnet, physical-user authenticity or IME qualification is claimed.
use super::*;
use fr_client::input::{
    InputClient, Policy,
    viewport::{SurfaceRect, Viewport},
};
use fr_core::{ids::*, input::*, limits::ProtocolLimits};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Mutex, atomic::AtomicBool},
    time::Instant,
};
static SERIAL: Mutex<()> = Mutex::new(());
struct Peer {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    window: Window,
}
impl Peer {
    fn new() -> Option<(Self, CString)> {
        let Ok(display) = std::env::var("DISPLAY") else {
            assert!(
                std::env::var_os("FR_NATIVE_INPUT_CAPTURE_REQUIRED").is_none(),
                "required X11 input tests need DISPLAY"
            );
            eprintln!("BLOCKED: no X11 display; native viewer input is not qualified");
            return None;
        };
        let mut child = Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/viewer_input/peer.py"
            ))
            .arg(&display)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let window = Window {
            id: line.trim().parse().unwrap(),
            width: 320,
            height: 240,
        };
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
    fn command(&mut self, s: &str) {
        assert_eq!(self.query(s), "ok");
    }
    fn query(&mut self, s: &str) -> String {
        writeln!(self.input, "{s}").unwrap();
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
fn capabilities() -> Capabilities {
    Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Repeat)
        .with(Capability::Absolute)
        .with(Capability::Buttons)
        .with(Capability::LineScroll)
}
fn viewport() -> (Viewport, Layout) {
    let bounds = InputBounds::new(DesktopPoint { x: -320, y: 0 }, 320, 240).unwrap();
    let client = InputClient::new(
        InputCredentials {
            session: RemoteSessionId::from_raw(1),
            lease: InputLeaseId::from_raw(2),
            ticket: InputTicketId::from_raw(3),
            view: InputView {
                geometry: DisplayGeometryGeneration::INITIAL,
                viewport: ViewportMappingGeneration::INITIAL,
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
        },
        7,
        bounds,
        capabilities(),
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(0),
    )
    .unwrap();
    let mut viewport = client.viewport();
    let layout = viewport
        .configure(bounds, SurfaceRect::new(0, 0, 320, 240).unwrap())
        .unwrap();
    viewport.confirm_layout(&layout).unwrap();
    (viewport, layout)
}
#[derive(Default)]
struct Probe {
    events: Mutex<Vec<(Event, ClientInstant)>>,
    ready: AtomicBool,
    stopped: AtomicBool,
    pause: AtomicBool,
    paused: AtomicBool,
    file_controls: AtomicBool,
    file_keymap: AtomicU8,
    file_commands: Mutex<Vec<file_keys::Command>>,
}
struct Recording {
    start: Instant,
    probe: Arc<Probe>,
    caps: Capabilities,
}
impl Target for Recording {
    fn file_shortcuts_enabled(&self) -> bool {
        self.probe.file_controls.load(Ordering::Acquire)
    }
    fn file_shortcut_map(&self, available: bool) {
        self.probe
            .file_keymap
            .store(if available { 1 } else { 2 }, Ordering::Release);
    }
    fn file_command(&self, command: file_keys::Command) {
        let mut commands = self.probe.file_commands.lock().unwrap();
        assert!(commands.len() < 16);
        commands.push(command);
    }
    fn clock(&self) -> Result<ClientInstant, StopReason> {
        if self.probe.stopped.load(Ordering::Acquire) {
            return Err(StopReason::Closed);
        }
        if self.probe.ready.load(Ordering::Acquire) && self.probe.pause.load(Ordering::Acquire) {
            self.probe.paused.store(true, Ordering::Release);
            while self.probe.pause.load(Ordering::Acquire) {
                thread::sleep(TURN);
            }
        }
        Ok(ClientInstant(
            1_000_000 + u64::try_from(self.start.elapsed().as_micros()).unwrap(),
        ))
    }
    fn capabilities(&self) -> Capabilities {
        self.caps
    }
    fn stop(&self, _: StopReason) {
        self.probe.stopped.store(true, Ordering::Release);
    }
    fn push(&mut self, event: Event, sampled: ClientInstant) -> Result<(), StopReason> {
        assert!(self.clock()?.0 - sampled.0 < 100_000);
        let mut events = self.probe.events.lock().unwrap();
        assert!(events.len() < 128);
        events.push((event, sampled));
        Ok(())
    }
    fn ready(&self) {
        self.probe.ready.store(true, Ordering::Release);
    }
}
fn wait(check: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(2);
    while !check() {
        assert!(Instant::now() < until, "native capture did not progress");
        thread::sleep(TURN);
    }
}
fn start(
    display: CString,
    window: Window,
    caps: Capabilities,
) -> (Arc<Probe>, JoinHandle<Result<(), StopReason>>) {
    start_using(display, window, caps, false)
}
fn start_using(
    display: CString,
    window: Window,
    caps: Capabilities,
    system_shortcuts: bool,
) -> (Arc<Probe>, JoinHandle<Result<(), StopReason>>) {
    let probe = Arc::new(Probe::default());
    let copied = probe.clone();
    let (_, layout) = viewport();
    let task = thread::spawn(move || {
        let mut target = Recording {
            start: Instant::now(),
            probe: copied,
            caps,
        };
        if system_shortcuts {
            run_with_system_shortcuts(&display, window, &layout, &mut target)
        } else {
            run(&display, window, &layout, &mut target)
        }
    });
    wait(|| probe.ready.load(Ordering::Acquire) || task.is_finished());
    assert!(
        !task.is_finished(),
        "native initialization failed: {:?}",
        task.join().unwrap()
    );
    (probe, task)
}
#[test]
fn physical_keys_pointer_buttons_and_wheel_preserve_native_order_and_original_layout() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, task) = start(display, peer.window, capabilities());
    for command in [
        "key 38 1",
        "key 38 0",
        "motion 40 50",
        "button 1 1",
        "button 1 0",
        "button 4 1",
        "button 4 0",
    ] {
        peer.command(command);
    }
    wait(|| probe.events.lock().unwrap().len() >= 6 || task.is_finished());
    assert!(!task.is_finished());
    probe.stopped.store(true, Ordering::Release);
    assert_eq!(task.join().unwrap(), Err(StopReason::Closed));
    let events = probe.events.lock().unwrap();
    assert!(events.windows(2).all(|v| v[0].1 <= v[1].1));
    assert!(
        matches!(&events[0].0,Event::Key{key,transition:KeyTransition::Press} if key.usage()==4)
    );
    assert!(
        matches!(&events[1].0,Event::Key{key,transition:KeyTransition::Release} if key.usage()==4)
    );
    let mut keys = 0;
    let mut buttons = 0;
    let mut scroll = 0;
    let mut motion = 0;
    for (event, _) in &*events {
        match event {
            Event::Key { .. } => keys += 1,
            Event::Pointer(_) => motion += 1,
            Event::Positioned {
                action:
                    PositionedAction::Button {
                        button: PointerButton::Primary,
                        ..
                    },
                ..
            } => buttons += 1,
            Event::Positioned {
                action:
                    PositionedAction::Scroll {
                        x: 0,
                        y: -65_536,
                        unit: ScrollUnit::Lines,
                    },
                ..
            } => scroll += 1,
            Event::HeldState(_) => {}
            _ => panic!("unexpected event category"),
        }
    }
    assert_eq!((keys, buttons, scroll), (2, 2, 1));
    assert!(motion >= 1);
}
#[test]
fn focus_window_keymap_and_synthetic_changes_stop_without_dispatching_the_changed_batch() {
    let _serial = SERIAL.lock().unwrap();
    for (command, reason) in [
        ("focus", StopReason::FocusLost),
        ("unmap", StopReason::WindowChanged),
        ("resize", StopReason::WindowChanged),
        ("remap", StopReason::KeymapChanged),
        ("synthetic", StopReason::SyntheticInput),
    ] {
        let Some((mut peer, display)) = Peer::new() else {
            return;
        };
        let (probe, task) = start(display, peer.window, capabilities());
        peer.command(command);
        wait(|| task.is_finished());
        let result = task.join().unwrap();
        // Unmapping may report focus loss first. Both fence the same Source.
        if command == "unmap" {
            assert!(matches!(
                result,
                Err(StopReason::FocusLost | StopReason::WindowChanged)
            ));
        } else {
            assert_eq!(result, Err(reason));
        }
        assert!(probe.events.lock().unwrap().is_empty());
    }
}
#[test]
fn repeat_capability_has_one_owner_and_missing_modes_are_not_emulated() {
    let (_, layout) = viewport();
    let mut names = [[0; 4]; 256];
    names[38] = *b"AC01";
    let mut decoder = Decoder::new(&names, capabilities());
    let press = Raw {
        kind: 1,
        detail: 38,
        ..Raw::default()
    };
    let release = Raw { kind: 2, ..press };
    assert!(matches!(
        decoder.event(press, &layout).unwrap(),
        Some(Event::Key {
            transition: KeyTransition::Press,
            ..
        })
    ));
    assert!(matches!(
        decoder.event(press, &layout).unwrap(),
        Some(Event::Key {
            transition: KeyTransition::Repeat,
            ..
        })
    ));
    assert!(matches!(
        decoder.event(release, &layout).unwrap(),
        Some(Event::Key {
            transition: KeyTransition::Release,
            ..
        })
    ));
    let mut decoder = Decoder::new(&names, Capabilities::default().with(Capability::Keys));
    assert!(decoder.event(press, &layout).unwrap().is_some());
    assert!(decoder.event(press, &layout).unwrap().is_none());
    assert!(decoder.event(release, &layout).unwrap().is_some());
    assert!(
        decoder
            .event(
                Raw {
                    kind: 3,
                    detail: 4,
                    ..press
                },
                &layout
            )
            .unwrap()
            .is_none()
    );
    assert!(
        decoder
            .event(Raw { kind: 5, ..press }, &layout)
            .unwrap()
            .is_none()
    );
}
#[test]
fn queued_native_timestamps_are_not_refreshed_at_dequeue_and_wrap_is_ordered() {
    let mut t = Timeline::new(u32::MAX - 10, ClientInstant(1_000_000));
    t.barrier(3).unwrap();
    assert_eq!(
        t.sample(u32::MAX - 5, 3, ClientInstant(1_020_000)),
        Ok(ClientInstant(1_010_000))
    );
    assert_eq!(
        t.sample(2, 3, ClientInstant(1_020_000)),
        Ok(ClientInstant(1_018_000))
    );
    assert_eq!(
        t.sample(1, 3, ClientInstant(1_020_000)),
        Err(StopReason::Clock)
    );
    assert_eq!(t.barrier(2), Err(StopReason::Clock));
    let mut t = Timeline::new(0, ClientInstant(0));
    t.barrier(200).unwrap();
    assert_eq!(
        t.sample(100, 200, ClientInstant(1_000_000)),
        Err(StopReason::Expired)
    );
    assert_eq!(
        t.sample(201, 200, ClientInstant(1_000_000)),
        Err(StopReason::Expired)
    );
}
#[test]
fn invalid_local_windows_and_preexisting_held_keys_refuse_before_event_forwarding() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (_, layout) = viewport();
    for display in ["", "host:0", ":0.0.0", ":x", ":000000", ":0\0"] {
        assert!(!local_display(display));
    }
    assert!(local_display(":99.0"));
    assert!(
        !Window {
            id: 0,
            ..peer.window
        }
        .valid(&layout)
    );
    assert!(
        !Window {
            width: 10,
            ..peer.window
        }
        .valid(&layout)
    );
    peer.command("key 38 1");
    let mut target = Recording {
        start: Instant::now(),
        probe: Arc::new(Probe::default()),
        caps: capabilities(),
    };
    // A key held when capture would start is named, not a native failure.
    assert_eq!(
        run(&display, peer.window, &layout, &mut target),
        Err(StopReason::InitialHeld)
    );
    peer.command("key 38 0");
    assert!(!target.probe.ready.load(Ordering::Acquire));
    assert!(target.probe.events.lock().unwrap().is_empty());
}

#[test]
fn native_backlog_after_a_stall_expires_without_freshening_or_forwarding_keys() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, task) = start(display, peer.window, capabilities());
    probe.pause.store(true, Ordering::Release);
    wait(|| probe.paused.load(Ordering::Acquire));
    peer.command("key 38 1");
    peer.command("key 38 0");
    thread::sleep(Duration::from_millis(120));
    probe.pause.store(false, Ordering::Release);
    wait(|| task.is_finished());
    assert_eq!(task.join().unwrap(), Err(StopReason::Expired));
    assert!(probe.events.lock().unwrap().is_empty());
}
#[test]
fn native_flood_is_bounded_before_queue_admission() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, task) = start(display, peer.window, capabilities());
    probe.pause.store(true, Ordering::Release);
    wait(|| probe.paused.load(Ordering::Acquire));
    peer.command("flood");
    probe.pause.store(false, Ordering::Release);
    wait(|| task.is_finished());
    assert_eq!(task.join().unwrap(), Err(StopReason::Overflow));
    assert!(probe.events.lock().unwrap().is_empty());
}

mod held;

mod escape;

mod wheel;

/// Live defect (2026-10-12): a controller window that mapped without keyboard
/// focus failed capture at once as `NativeFailure`, ending control within ~2 s
/// on a desktop whose window manager did not focus it. Capture now waits,
/// capturing nothing, until the window is focused...
#[test]
fn capture_waits_for_window_focus_before_opening() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    peer.command("focus");
    let probe = Arc::new(Probe::default());
    let copied = probe.clone();
    let (_, layout) = viewport();
    let window = peer.window;
    let task = thread::spawn(move || {
        run_focused(
            &display,
            window,
            &layout,
            &mut Recording {
                start: Instant::now(),
                probe: copied,
                caps: capabilities(),
            },
            Duration::from_secs(10),
        )
    });
    thread::sleep(Duration::from_millis(300));
    assert!(!task.is_finished());
    assert!(!probe.ready.load(Ordering::Acquire));
    assert!(probe.events.lock().unwrap().is_empty());
    peer.command("restore");
    wait(|| probe.ready.load(Ordering::Acquire) || task.is_finished());
    assert!(
        probe.ready.load(Ordering::Acquire),
        "{:?}",
        task.is_finished()
    );
    // Focus loss AFTER capture started still ends it, as before.
    peer.command("focus");
    assert_eq!(task.join().unwrap(), Err(StopReason::FocusLost));
}
/// ...but only for a bounded time: a window never focused ends as the named
/// `FocusLost`, still without capturing anything.
#[test]
fn capture_never_focused_ends_as_focus_lost() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    peer.command("focus");
    let (_, layout) = viewport();
    let mut target = Recording {
        start: Instant::now(),
        probe: Arc::new(Probe::default()),
        caps: capabilities(),
    };
    let started = Instant::now();
    assert_eq!(
        run_focused(
            &display,
            peer.window,
            &layout,
            &mut target,
            Duration::from_millis(400)
        ),
        Err(StopReason::FocusLost)
    );
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert!(!target.probe.ready.load(Ordering::Acquire));
    assert!(target.probe.events.lock().unwrap().is_empty());
    peer.command("restore");
}

#[path = "tests/file_keys.rs"]
mod file_keys_tests;

mod system_shortcuts;
