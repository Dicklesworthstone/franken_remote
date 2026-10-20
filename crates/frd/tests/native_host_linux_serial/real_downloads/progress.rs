//! Observe the shipped viewer's own panel through an independent Xlib peer.
//! The parent fixture supplies the actual commands, input and UDP/TLS lane;
//! its tailnet/ingress, Xvfb and window-manager evidence limits still apply.
use super::*;
use crate::{real_control::Harness, real_media::close_window};

const PROBE: &str = r#"
TITLE = b"FrankenRemote - download progress"
sig(x.XInternAtom, W, D, c.c_char_p, I)
sig(x.XGetWindowProperty, I, D, W, W, L, L, I, W, c.POINTER(W), c.POINTER(I),
    c.POINTER(W), c.POINTER(W), c.POINTER(c.c_void_p))
sig(x.XTranslateCoordinates, I, D, W, W, I, I, c.POINTER(I), c.POINTER(I), c.POINTER(W))
def rectangle(window):
    a, rx, ry, child = A(), I(), I(), W()
    assert x.XGetWindowAttributes(d, window, c.byref(a)) and a.map_state == 2
    assert a.root == root
    assert x.XTranslateCoordinates(d, window, root, 0, 0, c.byref(rx), c.byref(ry),
        c.byref(child))
    return rx.value - a.bw, ry.value - a.bw, a.w + 2 * a.bw, a.h + 2 * a.bw
def property32(window, name):
    typ, fmt, count, rest, data = W(), I(), W(), W(), c.c_void_p()
    atom = x.XInternAtom(d, name, 0)
    assert x.XGetWindowProperty(d, window, atom, 0, 16, 0, 0, c.byref(typ),
        c.byref(fmt), c.byref(count), c.byref(rest), c.byref(data)) == 0
    if not data.value:
        return []
    try:
        assert fmt.value == 32 and rest.value == 0
        return list(c.cast(data, c.POINTER(W))[:count.value])
    finally:
        x.XFree(data)
def command(cmd):
    if cmd[0] == "panel":
        panels = mapped(TITLE)
        assert len(panels) <= 1, "one panel for the original receive consent"
        if not panels:
            say("PANEL", "none")
            return
        window = panels[0][0]
        parent = property32(window, b"WM_TRANSIENT_FOR")
        hints = property32(window, b"WM_HINTS")
        assert len(parent) == 1
        nonfocus = len(hints) >= 2 and hints[0] & 1 and hints[1] == 0
        px, py, pw, ph = rectangle(window)
        vx, vy, vw, vh = rectangle(parent[0])
        _, _, sw, sh = rectangle(root)
        assert 0 <= px and 0 <= py and px + pw <= sw and py + ph <= sh
        assert px + pw <= vx or vx + vw <= px or py + ph <= vy or vy + vh <= py, \
            "the optional panel must not obscure the original input surface"
        say("PANEL", window, parent[0], "nonfocus" if nonfocus else "invalid")
    elif cmd[0] == "focus":
        focused, revert = W(), I()
        x.XGetInputFocus(d, c.byref(focused), c.byref(revert))
        say("FOCUS", focused.value)
    else:
        raise AssertionError("unknown panel probe command")
say("READY")
serve(lambda event: None, command)
"#;

fn panel(probe: &mut Harness, parent: u64) -> Option<u64> {
    let answer = probe.ask("panel");
    assert_eq!(answer[0], "PANEL");
    if answer[1] == "none" {
        assert_eq!(answer.len(), 2);
        return None;
    }
    assert_eq!(answer.len(), 4);
    assert_eq!(answer[2].parse::<u64>().unwrap(), parent);
    assert_eq!(answer[3], "nonfocus");
    Some(answer[1].parse().unwrap())
}

fn focused(probe: &mut Harness, parent: u64) {
    let answer = probe.ask("focus");
    assert_eq!(answer[0], "FOCUS");
    assert_eq!(answer[1].parse::<u64>().unwrap(), parent);
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real frd/fr CLIs, download panel, input agent and file lane"]
fn hiding_the_download_panel_preserves_publication_and_the_original_controller() {
    let source = Dir::new("download-panel-source");
    let destination = Dir::new("download-panel-destination");
    let names = [
        "01-before-hiding-private.bin",
        "02-after-hiding-private.bin",
    ];
    let first = random(32 * 1024);
    let second = random(3 * 1024 * 1024);
    fs::write(source.0.join(names[0]), &first).unwrap();
    fs::write(source.0.join(names[1]), &second).unwrap();
    let selected: Vec<_> = names.iter().map(|name| source.0.join(name)).collect();
    let mut s = Controlled::start_downloads(false, false, &selected, Some(&destination.0));
    let original_indicator = indicator(&mut s.observer).unwrap();
    let mut probe = Harness::start(PROBE, &s.viewer.display, "READY");
    let mut shown = None;
    assert!(eventually(Duration::from_secs(10), || {
        shown = panel(&mut probe, s.window.0);
        shown.is_some()
    }));
    focused(&mut probe, s.window.0);
    assert!(
        eventually(Duration::from_secs(60), || destination
            .0
            .join(names[0])
            .exists()),
        "first download did not publish: {}; host: {}",
        client_state(&mut s.client),
        s.daemon.dump()
    );
    assert_file(&destination.0.join(names[0]), &first);
    // A normal window-manager close hides only the panel. The second object
    // may finish before or after this event; no scheduling claim is required.
    close_window(&s.viewer.display, shown.unwrap());
    assert!(eventually(Duration::from_secs(10), || {
        panel(&mut probe, s.window.0).is_none()
    }));
    focused(&mut probe, s.window.0);
    assert!(s.pointer_follows((180, 180), Duration::from_secs(10)));
    assert!(
        eventually(Duration::from_secs(90), || destination
            .0
            .join(names[1])
            .exists()),
        "hiding the panel stopped the original download: {}; host: {}",
        client_state(&mut s.client),
        s.daemon.dump()
    );
    assert_file(&destination.0.join(names[0]), &first);
    assert_file(&destination.0.join(names[1]), &second);
    assert_eq!(destination.entries(), names);
    assert_eq!(indicator(&mut s.observer), Some(original_indicator));
    assert!(s.pointer_follows((220, 180), Duration::from_secs(10)));
    assert_eq!(panel(&mut probe, s.window.0), None);
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let host = s.daemon.finish_once();
    let report = completion(&output, &host);
    assert!(output.status.success(), "{report}; {host}");
    assert_download_report(&report);
    assert_eq!(report["files_receive_cancel_requested"], false, "{report}");
    assert_eq!(report["files_receive_cleanup"], "finished", "{report}");
    assert_eq!(
        report["files_receive_refused"],
        serde_json::json!([]),
        "{report}"
    );
    assert_eq!(
        report["files_received"],
        serde_json::json!([
            {"index": 0, "bytes": first.len(), "durable": true},
            {"index": 1, "bytes": second.len(), "durable": true}
        ]),
        "{report}"
    );
    assert_private(&output, &host, &[&source.0, &destination.0], &names);
}
