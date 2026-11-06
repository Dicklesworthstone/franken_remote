//! Actual X11 panel, gesture attribution and independent capture guard. The
//! explicit Xvfb slave is a hardware-event fixture, not proof of human consent
//! or physical microphone capture. No network or authority mocks run here.
#![cfg(all(target_os = "linux", feature = "linux-input"))]
#![forbid(unsafe_code)]
use fr_native::sharing_indicator::{
    self, IndicatorControl, MicrophonePanel, MicrophoneState, Status, StopReason,
};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

static SERIAL: Mutex<()> = Mutex::new(());
fn display() -> Option<String> {
    match std::env::var("DISPLAY") {
        Ok(display) => Some(display),
        Err(_) if std::env::var_os("FR_NATIVE_MICROPHONE_UI_REQUIRED").is_none() => {
            eprintln!("BLOCKED: microphone UI requires X11; no native qualification");
            None
        }
        Err(error) => panic!("required native microphone display missing: {error}"),
    }
}
fn until(mut check: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(3);
    while !check() {
        assert!(
            Instant::now() < end,
            "native microphone UI did not progress"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn peer(display: &str, control: &IndicatorControl, op: &str, args: &[&str]) {
    assert!(
        Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/sharing_indicator/peer.py"
            ))
            .arg(display)
            .arg(control.window().unwrap().to_string())
            .arg(op)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}
fn ready(control: &IndicatorControl) {
    until(|| control.status() != Status::Opening);
    assert_eq!(control.status(), Status::Mapped);
    assert!(control.responsive());
}

#[test]
fn muted_first_talk_requires_a_complete_attributed_click_and_mute_fences_without_a_turn() {
    let _serial = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    let mut panel =
        MicrophonePanel::start(&display, move |_| flag.store(true, Ordering::Release)).unwrap();
    let control = panel.control();
    ready(&control);
    let check = panel.talk_check();
    let epoch = panel.talk_epoch();
    assert!(!check());
    assert!(panel.publish(MicrophoneState::Live));
    assert!(!check(), "a label cannot activate recording");
    assert!(panel.publish(MicrophoneState::Muted));
    for op in ["click-at", "synthetic-at", "device-release-at"] {
        peer(&display, &control, op, &["75", "95"]);
        thread::sleep(Duration::from_millis(30));
        assert!(
            !panel.take_talk_request(),
            "unattributed/partial click: {op}"
        );
        assert!(!check());
    }
    peer(&display, &control, "device-press-at", &["75", "95"]);
    thread::sleep(Duration::from_millis(30));
    assert!(!panel.take_talk_request());
    peer(&display, &control, "device-release-at", &["75", "95"]);
    until(|| panel.take_talk_request());
    let first_token = epoch().unwrap();
    assert!(check());
    assert!(!panel.take_talk_request());
    assert!(panel.publish(MicrophoneState::Live));
    peer(&display, &control, "device-click-at", &["230", "95"]);
    until(|| !check());
    assert!(
        !stopped.load(Ordering::Acquire),
        "mute preserves independent attachment"
    );
    assert!(panel.take_mute_request());
    assert!(!panel.take_mute_request());
    peer(&display, &control, "device-click-at", &["75", "95"]);
    until(|| panel.take_talk_request());
    assert_ne!(epoch(), Some(first_token));
    assert!(check());
    peer(&display, &control, "device-click-at", &["370", "95"]);
    until(|| stopped.load(Ordering::Acquire));
    assert!(!check());
    until(|| panel.finish().is_some());
    assert_eq!(panel.finish(), Some(StopReason::User));
}

#[test]
fn hiding_a_talking_panel_fences_before_the_session_can_publish_or_drain() {
    let _serial = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    let mut panel =
        MicrophonePanel::start(&display, move |_| flag.store(true, Ordering::Release)).unwrap();
    let control = panel.control();
    ready(&control);
    let check = panel.talk_check();
    peer(&display, &control, "device-click-at", &["75", "95"]);
    until(|| panel.take_talk_request());
    assert!(check());
    peer(&display, &control, "unmap", &[]);
    until(|| stopped.load(Ordering::Acquire));
    assert!(!check());
    assert!(!panel.publish(MicrophoneState::Live));
    assert!(!panel.take_talk_request());
    until(|| panel.finish().is_some());
    assert_eq!(panel.finish(), Some(StopReason::Hidden));
}

#[test]
fn host_microphone_surface_revokes_only_its_supplied_owner() {
    let _serial = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let stopped = Arc::new(AtomicBool::new(false));
    let foreign = Arc::new(AtomicBool::new(false));
    let other = foreign.clone();
    let mut observation = sharing_indicator::start_observation_with(&display, move |_| {
        other.store(true, Ordering::Release);
    })
    .unwrap();
    ready(&observation.control());
    let flag = stopped.clone();
    let mut panel = sharing_indicator::start_microphone_with(&display, move |_| {
        flag.store(true, Ordering::Release);
    })
    .unwrap();
    let control = panel.control();
    ready(&control);
    peer(&display, &control, "snapshot", &[]);
    peer(&display, &control, "device-click", &[]);
    until(|| stopped.load(Ordering::Acquire));
    assert!(!foreign.load(Ordering::Acquire));
    assert!(observation.control().responsive());
    until(|| panel.finish().is_some());
    assert_eq!(panel.finish(), Some(StopReason::User));
    observation.control().stop();
    until(|| observation.finish().is_some());
}

struct Viewer {
    child: Child,
    target: fr_media::worker::presentation::X11Target,
}
impl Viewer {
    fn start(display: &str, width: u32, height: u32) -> Self {
        const SOURCE: &str = r"
import ctypes as c, sys
x = c.CDLL('libX11.so.6')
def f(name, result, *args):
    value = getattr(x, name)
    value.restype, value.argtypes = result, list(args)
    return value
D, W = c.c_void_p, c.c_ulong
f('XOpenDisplay', D, c.c_char_p)
f('XDefaultRootWindow', W, D)
f('XDisplayWidth', c.c_int, D, c.c_int)
f('XDisplayHeight', c.c_int, D, c.c_int)
f('XCreateSimpleWindow', W, D, W, c.c_int, c.c_int, c.c_uint, c.c_uint, c.c_uint, W, W)
f('XMapWindow', c.c_int, D, W)
f('XResizeWindow', c.c_int, D, W, c.c_uint, c.c_uint)
f('XSync', c.c_int, D, c.c_int)
f('XCloseDisplay', c.c_int, D)
d = x.XOpenDisplay(sys.argv[1].encode())
assert d
width = int(sys.argv[2]) or x.XDisplayWidth(d, 0)
height = int(sys.argv[3]) or x.XDisplayHeight(d, 0)
w = x.XCreateSimpleWindow(d, x.XDefaultRootWindow(d), 0, 0, width, height, 0, 0, 0)
assert w
x.XMapWindow(d, w)
x.XSync(d, 0)
print(w, width, height, flush=True)
for line in sys.stdin:
    if line.strip() != 'resize': break
    x.XResizeWindow(d, w, width + 1, height)
    x.XSync(d, 0)
x.XCloseDisplay(d)
";
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(SOURCE)
            .arg(display)
            .arg(width.to_string())
            .arg(height.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let parts: Vec<u32> = line
            .split_whitespace()
            .map(|part| part.parse().unwrap())
            .collect();
        assert_eq!(parts.len(), 3);
        let target =
            fr_media::worker::presentation::X11Target::new(parts[0], parts[1], parts[2]).unwrap();
        Self { child, target }
    }
    fn resize(&mut self) {
        self.child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"resize\n")
            .unwrap();
    }
}
impl Drop for Viewer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn exact_viewer_layout_is_retained_and_fullscreen_without_room_refuses() {
    let _serial = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let mut viewer = Viewer::start(&display, 320, 240);
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    let mut panel = MicrophonePanel::start_for_viewer(&display, viewer.target, move |_| {
        flag.store(true, Ordering::Release);
    })
    .unwrap();
    let control = panel.control();
    ready(&control);
    peer(&display, &control, "device-click-at", &["75", "95"]);
    until(|| panel.take_talk_request());
    let epoch = panel.talk_epoch();
    assert!(epoch().is_some());
    viewer.resize();
    until(|| stopped.load(Ordering::Acquire));
    assert_eq!(epoch(), None);
    until(|| panel.finish().is_some());
    assert_eq!(panel.finish(), Some(StopReason::Hidden));
    drop(viewer);

    let viewer = Viewer::start(&display, 0, 0);
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    let mut panel = MicrophonePanel::start_for_viewer(&display, viewer.target, move |_| {
        flag.store(true, Ordering::Release);
    })
    .unwrap();
    until(|| stopped.load(Ordering::Acquire));
    assert_eq!(panel.talk_epoch()(), None);
    until(|| panel.finish().is_some());
    assert_eq!(panel.finish(), Some(StopReason::NativeFailure));
}
