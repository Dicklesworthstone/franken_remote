//! Real native privacy controls terminate only the original clipboard lane.
use super::{
    Controlled, Harness, Peer, clipboard_children, completion, eventually, pasted, unique, wait_for,
};
use std::{thread, time::Duration};

const SWITCH: &str = r#"
sig(x.XUnmapWindow, I, D, W)
def command(cmd):
    found = mapped(TITLE)
    if cmd[0] == "window":
        say("WINDOW", *(found[0] if found else ("none",)))
    elif found and cmd[0] == "hide":
        x.XUnmapWindow(d, found[0][0])
        x.XSync(d, 0)
        say("OK")
    elif found and cmd[0] == "off":
        # The client panel's Turn off button. XTest is an independent native
        # user harness here; it is not a physical-device qualification claim.
        w, px, py, width, height = found[0]
        assert width == 640 and height == 310
        t.XTestFakeMotionEvent(d, 0, px + 500, py + 250, 0)
        t.XTestFakeButtonEvent(d, 1, 1, 0)
        t.XTestFakeButtonEvent(d, 1, 0, 0)
        x.XSync(d, 0)
        say("OK")
    else:
        say("MISSING")
say("SWITCH")
serve(lambda e: None, command)
"#;

#[derive(Clone, Copy)]
enum LocalAction {
    HideHost,
    HideClient,
    TurnOffClient,
}

fn exercise(action: LocalAction) {
    // Neither endpoint passes --clipboard: this is the default-on CLI path.
    let mut session = Controlled::start_with(true, true);
    let mut host = Peer::start(&session.host.display);
    let mut viewer = Peer::start(&session.viewer.display);
    let seed = unique("before privacy off");
    viewer.copy(&seed);
    assert!(pasted(&mut host, &seed, Duration::from_secs(20)).is_ok());
    // Exercise an idle lane after the successful transfer has been acknowledged.
    // A stop during retained protected output has a distinct connection-wide
    // privacy fence, covered by the transport's retire_io regressions.
    thread::sleep(Duration::from_secs(2));

    let (title, display, command) = match action {
        LocalAction::HideHost => (
            "FrankenRemote - Clipboard sharing",
            &session.host.display,
            "hide",
        ),
        LocalAction::HideClient | LocalAction::TurnOffClient => (
            "FrankenRemote - shared clipboard",
            &session.viewer.display,
            if matches!(action, LocalAction::HideClient) {
                "hide"
            } else {
                "off"
            },
        ),
    };
    let mut switch = Harness::start(&format!("TITLE = b'{title}'\n{SWITCH}"), display, "SWITCH");
    assert!(eventually(Duration::from_secs(5), || switch.ask("window")
        [1]
        != "none"));
    assert_eq!(switch.ask(command), ["OK"]);
    assert!(
        eventually(Duration::from_secs(15), || clipboard_children() == 0),
        "the original host clipboard owner survived local privacy-off"
    );

    // New selections remain local. Cleanup must not erase either newer value,
    // and neither direction may restart under the still-running input lease.
    let host_text = unique("host stays local");
    let viewer_text = unique("viewer stays local");
    host.copy(&host_text);
    viewer.copy(&viewer_text);
    assert!(session.pointer_follows((220, 180), Duration::from_secs(10)));
    for _ in 0..20 {
        assert_eq!(host.paste().as_deref(), Some(host_text.as_str()));
        assert_eq!(viewer.paste().as_deref(), Some(viewer_text.as_str()));
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(clipboard_children(), 0, "clipboard silently re-enabled");
    session.close_viewer();
    let output = wait_for(session.client, Duration::from_secs(30));
    let report = completion(&output, &session.daemon.dump());
    assert_eq!(report["control_granted"], true, "{report}");
    assert_eq!(report["clipboard_requested"], true, "{report}");
    assert_eq!(report["clipboard_active"], true, "{report}");
    assert!(report["clipboard_ended"].is_string(), "{report}");
    assert_eq!(
        report["clipboard_absence"],
        serde_json::Value::Null,
        "{report}"
    );
    session.daemon.finish();
}

#[test]
#[ignore = "explicit isolated namespace; two Xvfb displays; real input agent and clipboard child"]
fn hiding_the_host_clipboard_switch_stops_only_clipboard_when_idle() {
    exercise(LocalAction::HideHost);
}

#[test]
#[ignore = "explicit isolated namespace; two Xvfb displays; real input agent and clipboard child"]
fn hiding_the_client_clipboard_switch_stops_only_clipboard_when_idle() {
    exercise(LocalAction::HideClient);
}

#[test]
#[ignore = "explicit isolated namespace; two Xvfb displays; real input agent and clipboard child"]
fn turning_off_the_idle_client_clipboard_keeps_control_and_new_local_values() {
    exercise(LocalAction::TurnOffClient);
}
