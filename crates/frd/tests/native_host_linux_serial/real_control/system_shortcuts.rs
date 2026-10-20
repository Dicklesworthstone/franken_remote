//! Shipped `fr`/`frd` keyboard capture on two real X11 servers, including
//! independent host input effects and a competing local passive-shortcut owner.
//! Xvfb/XTest and the parent's tailnet/ingress fixtures retain their stated
//! evidence limits; these are not physical-device or window-manager guarantees.
use super::*;

/// This peer never selects ordinary key events on the root. Its key notices
/// therefore prove real passive grabs, independently of FrankenRemote's active
/// keyboard owner. The same bindings must work after that owner has ended.
const PROBE: &str = r#"
ACTIVE = b"FrankenRemote - Remote system shortcuts - Ctrl+Alt+Shift+Escape releases control"
sig(x.XGrabKey, I, D, I, U, W, I, I, I)
sig(x.XGrabKeyboard, I, D, W, I, I, I, W)
sig(x.XUngrabKeyboard, I, D, W)
class Modifiers(c.Structure):
    _fields_ = [("max_keypermod", I), ("modifiermap", c.POINTER(c.c_ubyte))]
sig(x.XGetModifierMapping, c.POINTER(Modifiers), D)
sig(x.XFreeModifiermap, I, c.POINTER(Modifiers))
errors = []
CHECK = ERRORS(lambda display, error: errors.append("x_error") or 0)
x.XSetErrorHandler(CHECK)
codes = {name: int(x.XKeysymToKeycode(d, keysym)) for name, keysym in
         (("alt", 0xffe9), ("tab", 0xff09), ("super", 0xffeb),
          ("ctrl", 0xffe3), ("shift", 0xffe1), ("escape", 0xff1b), ("a", 0x61))}
assert all(codes.values()) and len(set(codes.values())) == len(codes)
mapping = x.XGetModifierMapping(d)
assert mapping
try:
    count = mapping.contents.max_keypermod
    assert 0 < count <= 32
    alt_mask = 0
    for modifier in range(8):
        if any(mapping.contents.modifiermap[modifier * count + i] == codes["alt"]
               for i in range(count)):
            alt_mask |= 1 << modifier
    assert alt_mask and not alt_mask & (alt_mask - 1)
finally:
    x.XFreeModifiermap(mapping)
# Asynchronous passive grabs cannot freeze the server or take focus by setup.
x.XGrabKey(d, codes["tab"], alt_mask, root, 0, 1, 1)
x.XGrabKey(d, codes["super"], 0, root, 0, 1, 1)
x.XSync(d, 0)
assert not errors, errors
counts = {(code, kind): 0 for code in (codes["tab"], codes["super"]) for kind in (2, 3)}
def event(e):
    if e.type in (2, 3) and e.input.window == root:
        assert not e.input.send_event
        key = (e.input.detail, e.type)
        if key in counts:
            counts[key] += 1
            assert sum(counts.values()) <= 128
def drain():
    x.XSync(d, 0)
    consumed = 0
    while x.XPending(d):
        e = Event()
        x.XNextEvent(d, c.byref(e))
        event(e)
        consumed += 1
        assert consumed <= 128
    assert not errors, errors
def chord(names):
    for name in names:
        assert t.XTestFakeKeyEvent(d, codes[name], 1, 0)
    for name in reversed(names):
        assert t.XTestFakeKeyEvent(d, codes[name], 0, 0)
def command(cmd):
    if cmd[0] == "active":
        window = int(cmd[1])
        assert title(window) == ACTIVE
        a, focused, revert = A(), W(), I()
        assert x.XGetWindowAttributes(d, window, c.byref(a)) and a.map_state == 2
        x.XGetInputFocus(d, c.byref(focused), c.byref(revert))
        assert focused.value == window
        say("ACTIVE", window)
    elif cmd[0] == "grab":
        status = x.XGrabKeyboard(d, root, 0, 1, 1, 0)
        if status == 0:
            x.XUngrabKeyboard(d, 0)
        x.XSync(d, 0)
        assert not errors, errors
        say("GRAB", status)
    elif cmd[0] == "local":
        drain()
        say("LOCAL", *(counts[(codes[name], kind)] for name in ("tab", "super")
                       for kind in (2, 3)))
    elif cmd[0] in ("shortcuts", "ordinary", "escape"):
        if cmd[0] == "shortcuts":
            chord(("alt", "tab"))
            chord(("super",))
        elif cmd[0] == "ordinary":
            chord(("a",))
        else:
            chord(("ctrl", "alt", "shift", "escape"))
        x.XSync(d, 0)
        assert not errors, errors
        say("OK")
    else:
        raise AssertionError("unknown shortcut probe command")
say("READY")
serve(event, command)
"#;

fn remote_shortcuts_and_ordinary_input(s: &mut Controlled, probe: &mut Harness) {
    let original = indicator(&mut s.observer).unwrap();
    assert_eq!(
        probe.ask(&format!("active {}", s.window.0)),
        ["ACTIVE".to_owned(), s.window.0.to_string()]
    );
    // AlreadyGrabbed: this independent client cannot steal the original grab.
    assert_eq!(probe.ask("grab"), ["GRAB", "1"]);
    let host_keys = [0xffe9, 0xff09, 0xffeb, 0x61].map(|keysym| {
        let answer = s.observer.ask(&format!("keycode {keysym}"));
        assert_eq!(answer[0], "CODE");
        let code: u32 = answer[1].parse().unwrap();
        assert_ne!(code, 0);
        code
    });
    let before = host_events(&mut s.observer).len();
    assert_eq!(probe.ask("shortcuts"), ["OK"]);
    assert_eq!(probe.ask("ordinary"), ["OK"]);
    assert!(
        eventually(Duration::from_secs(10), || {
            let events = host_events(&mut s.observer);
            host_keys.iter().all(|&code| {
                [2, 3].iter().all(|&kind| {
                    events[before..]
                        .iter()
                        .any(|e| (e.0, e.1, e.3) == (kind, code, false))
                })
            })
        }),
        "captured shortcuts did not reach the real host input agent: {:?}; {}",
        host_events(&mut s.observer),
        s.daemon.dump()
    );
    assert_eq!(probe.ask("local"), ["LOCAL", "0", "0", "0", "0"]);
    assert_eq!(indicator(&mut s.observer), Some(original));
    assert!(s.pointer_follows((190, 240), Duration::from_secs(10)));
    assert_eq!(probe.ask("grab"), ["GRAB", "1"]);
}

#[derive(Clone, Copy, Debug)]
enum Exit {
    Window,
    Escape,
}

fn stop_and_prove_local_recovery(mut s: Controlled, mut probe: Harness, exit: Exit) {
    let escape: u32 = s.observer.ask("keycode 0xff1b")[1].parse().unwrap();
    let before = host_events(&mut s.observer).len();
    match exit {
        Exit::Window => s.close_viewer(),
        Exit::Escape => assert_eq!(probe.ask("escape"), ["OK"]),
    }
    let output = wait_for(s.client, Duration::from_secs(30));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
        panic!(
            "fr ({exit:?}): {stdout} {}; host: {}",
            String::from_utf8_lossy(&output.stderr),
            s.daemon.dump()
        )
    });
    match exit {
        Exit::Window => {
            assert!(output.status.success(), "{report}");
            assert_eq!(report["outcome"], "stopped", "{report}");
            assert_eq!(report["role"], "control", "{report}");
            assert_eq!(report["control_requested"], true, "{report}");
            assert_eq!(report["control_granted"], true, "{report}");
            assert!(report["input_submitted_to_os"].as_u64().unwrap() >= 8);
            controller_closed(&report);
        }
        Exit::Escape => {
            assert_eq!(output.status.code(), Some(1), "{report}");
            assert_eq!(
                report["error"]["code"], "control_released_locally",
                "{report}"
            );
        }
    }
    assert!(eventually(Duration::from_secs(15), || {
        indicator(&mut s.observer).is_none()
    }));
    assert!(
        host_events(&mut s.observer)[before..]
            .iter()
            .all(|e| !matches!(e.0, 2 | 3) || e.1 != escape),
        "the local emergency key reached the host"
    );
    // Success from an independent X connection proves the old native owner
    // released the grab. The probe immediately releases its own test grab.
    assert_eq!(probe.ask("grab"), ["GRAB", "0"]);
    assert_eq!(probe.ask("local"), ["LOCAL", "0", "0", "0", "0"]);
    // Planted positive: the same passive root bindings now receive real keys.
    // This rules out a broken/absent local shortcut observer during capture.
    assert_eq!(probe.ask("shortcuts"), ["OK"]);
    assert!(eventually(Duration::from_secs(5), || {
        probe.ask("local") == ["LOCAL", "1", "1", "1", "1"]
    }));
    s.daemon.finish();
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real fr/frd, keyboard grabs and input agent"]
fn captured_system_shortcuts_reach_the_host_and_release_on_both_local_exits() {
    for exit in [Exit::Window, Exit::Escape] {
        let mut s = Controlled::start_system_shortcuts();
        let mut probe = Harness::start(PROBE, &s.viewer.display, "READY");
        remote_shortcuts_and_ordinary_input(&mut s, &mut probe);
        stop_and_prove_local_recovery(s, probe, exit);
    }
}
