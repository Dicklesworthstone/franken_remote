//! Actual private D-Bus -> canonical native input thread -> independent X11
//! observation. Logind metadata/approval are fixtures; `XTest` effects are real.
use super::*;
use crate::input_agent::{Error as StartError, start_x11_guarded};
use asupersync::{runtime::RuntimeBuilder, types::Budget};
use fr_core::{
    input::{DesktopPoint, InputEvent, InputRequest, KeyTransition, PhysicalKey, PointerButton},
    input_sequence::InputOutcome,
    input_submission::Dispatch,
    limits::ProtocolLimits,
};
use fr_wire::input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, encode_input};
use frd::input_agent::{Agent, Error as AgentError, Reply, Route, Seat};
use std::{
    ffi::{CString, c_char, c_int, c_uint, c_ulong, c_void},
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    ptr::NonNull,
    sync::mpsc,
};
// Test-only declarations from Xlib.h. This independent connection belongs to the
// test thread; the production input connection belongs to its native worker.
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XDefaultRootWindow(display: *mut c_void) -> c_ulong;
    fn XKeysymToKeycode(display: *mut c_void, key: c_ulong) -> u8;
    fn XQueryKeymap(display: *mut c_void, keys: *mut u8) -> c_int;
    fn XQueryPointer(
        display: *mut c_void,
        window: c_ulong,
        root: *mut c_ulong,
        child: *mut c_ulong,
        root_x: *mut c_int,
        root_y: *mut c_int,
        win_x: *mut c_int,
        win_y: *mut c_int,
        mask: *mut c_uint,
    ) -> c_int;
}
struct Xserver {
    child: Child,
    name: String,
    observer: NonNull<c_void>,
    keycode: u8,
}
impl Xserver {
    fn new() -> Self {
        crate::xlib::initialize_threads().unwrap();
        let mut child = Command::new("/usr/bin/Xvfb")
            .env_clear()
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-nolisten",
                "tcp",
                "-noreset",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut name = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut name)
            .unwrap();
        assert_ne!(name.trim(), "");
        let name = format!(":{}", name.trim());
        let text = CString::new(name.as_bytes()).unwrap();
        let observer = NonNull::new(unsafe { XOpenDisplay(text.as_ptr()) }).unwrap();
        let keycode = unsafe { XKeysymToKeycode(observer.as_ptr(), 0xffe1) }; // Shift_L
        assert!(keycode >= 8);
        Self {
            child,
            name,
            observer,
            keycode,
        }
    }
    fn held(&self) -> (bool, bool) {
        let mut keys = [0_u8; 32];
        assert_ne!(
            unsafe { XQueryKeymap(self.observer.as_ptr(), keys.as_mut_ptr()) },
            0
        );
        let key = keys[usize::from(self.keycode / 8)] & (1 << (self.keycode % 8)) != 0;
        let mut root = 0;
        let mut child = 0;
        let (mut rx, mut ry, mut wx, mut wy, mut mask) = (0, 0, 0, 0, 0);
        assert_ne!(
            unsafe {
                XQueryPointer(
                    self.observer.as_ptr(),
                    XDefaultRootWindow(self.observer.as_ptr()),
                    &raw mut root,
                    &raw mut child,
                    &raw mut rx,
                    &raw mut ry,
                    &raw mut wx,
                    &raw mut wy,
                    &raw mut mask,
                )
            },
            0
        );
        (key, mask & (1 << 8) != 0) // Button1Mask, independent of submission receipt.
    }
}
impl Drop for Xserver {
    fn drop(&mut self) {
        unsafe {
            XCloseDisplay(self.observer.as_ptr());
        }
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}
fn bytes(sequence: u64, event: InputEvent<'_>) -> Vec<u8> {
    let mut bytes = vec![0; MAX_INPUT_RECORD_BYTES];
    let n = encode_input(
        InputRequest {
            credentials: input_support::credentials(),
            sequence,
            event,
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}
fn key(pressed: bool) -> InputEvent<'static> {
    InputEvent::Key {
        key: PhysicalKey::new(0xe1).unwrap(),
        transition: if pressed {
            KeyTransition::Press
        } else {
            KeyTransition::Release
        },
    }
}
fn reply(agent: &mut Agent) -> Reply {
    let mut reply = None;
    wait(|| {
        reply = agent.try_reply().unwrap();
        reply.is_some()
    });
    reply.unwrap()
}
#[derive(Clone, Copy)]
enum End {
    Lock,
    LostReplies,
    Abandon,
}
fn run(end: End) {
    let _serial = SERIAL.lock().unwrap();
    let x = Xserver::new();
    let peer = Peer::new(Data {
        uid: uid(),
        display: x.name.clone(),
        ..Data::default()
    });
    let mut selection = selection();
    selection.uid = uid();
    selection.display.clone_from(&x.name);
    let mut watch = Watch::spawn(selection, peer.address.clone(), uid()).unwrap();
    active(&watch);
    let rt = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let session = input_support::session(&cx);
    let monitor = session.monitor();
    let seat = Seat::default();
    let route = Route::new(7, ProtocolLimits::ABSOLUTE);
    let (mut agent, driver) =
        start_x11_guarded(&seat, cx, session, route, &x.name, watch.control()).unwrap();
    let mut driver = Some(driver);
    let (done, shutdown) = mpsc::sync_channel(1);
    let runner = if matches!(end, End::Abandon) {
        None
    } else {
        let driver = driver.take().unwrap();
        Some(thread::spawn(move || {
            done.send(rt.block_on(driver)).unwrap();
        }))
    };
    assert_eq!(x.held(), (false, false));
    for (seq, event) in [
        key(true),
        InputEvent::Button {
            button: PointerButton::Primary,
            pressed: true,
            position: DesktopPoint { x: 10, y: 10 },
            barrier: 0,
        },
    ]
    .into_iter()
    .enumerate()
    {
        agent
            .submit(&bytes(seq as u64, event), InputDelivery::Reliable)
            .unwrap();
        let Reply::Input(Ok(Dispatch::Completed(receipt))) = reply(&mut agent) else {
            panic!("missing native receipt")
        };
        assert_eq!(receipt.outcome, InputOutcome::SubmittedToOs);
    }
    // OS observation is deliberately separate and bounded, never action retry.
    wait(|| x.held() == (true, true));
    match end {
        End::Lock => peer.emit(Event::LockUnlock),
        End::LostReplies => peer.data.lock().unwrap().no_reply = true,
        End::Abandon => drop(driver.take()),
    }
    if let Some(runner) = runner {
        let result = shutdown.recv_timeout(Duration::from_secs(4)).unwrap();
        runner.join().unwrap();
        assert!(
            result.handoff_safe(),
            "native cleanup must be observed: {result:?}"
        );
        assert_eq!(result.exit.unwrap().cleanup.unwrap().submitted_releases, 2);
        if matches!(end, End::Lock) {
            assert_eq!(result.reason, frd::input_watchdog::StopReason::Suspended);
            assert_eq!(watch.control.status(), Status::Stopped(StopReason::Locked));
        }
    } else {
        // An abandoned future has no Shutdown receipt; observe worker exit via
        // its original Agent rather than pretending Drop synchronously joined.
        wait(|| agent.status().exit.is_some());
        assert!(agent.status().exit.unwrap().handoff_safe());
    }
    assert!(monitor.is_revoked());
    wait(|| !seat.is_occupied());
    wait(|| x.held() == (false, false));
    assert_eq!(
        agent.submit(&bytes(2, key(true)), InputDelivery::Reliable),
        Err(AgentError::Stopped)
    );
    finish(&mut watch);
}
#[test]
fn logind_lock_releases_real_x11_keys_and_buttons_without_another_input_packet() {
    run(End::Lock);
}
#[test]
fn lost_logind_evidence_releases_real_x11_state_on_original_watchdog() {
    run(End::LostReplies);
}
#[test]
fn unpolled_guarded_driver_abandonment_reaps_the_original_native_input_owner() {
    run(End::Abandon);
}

#[test]
fn wrong_logind_input_uid_or_display_never_reserves_a_native_seat() {
    let _serial = SERIAL.lock().unwrap();
    for wrong_uid in [false, true] {
        let selected = Selection {
            uid: if wrong_uid { uid() + 1 } else { uid() },
            ..selection()
        };
        let evidence = Control(Arc::new(Shared {
            selection: selected,
            state: AtomicU8::new(1),
            deadline: AtomicU64::new(bus::boottime().unwrap() + VALID_NS),
            waker: Mutex::new(None),
        }));
        let rt = RuntimeBuilder::new().worker_threads(1).build().unwrap();
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let seat = Seat::default();
        let result = start_x11_guarded(
            &seat,
            cx.clone(),
            input_support::session(&cx),
            Route::new(7, ProtocolLimits::ABSOLUTE),
            if wrong_uid { ":7" } else { ":8" },
            evidence,
        );
        assert!(matches!(result, Err(StartError::InvalidDisplay)));
        assert!(!seat.is_occupied());
    }
}
