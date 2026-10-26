//! The same shipped client and identity through a kernel route failure.
//! Tailnet admission and displays are the existing namespace fixtures.
use super::super::real_files::Dir;
use super::super::real_media::{near, window_pixels};
use super::*;

/// Change only the namespace fixture's host route. Keep its exact prior route
/// attributes for restoration, including on an assertion failure.
struct RouteFailure {
    original: Vec<String>,
    restored: bool,
}
impl RouteFailure {
    fn install() -> Self {
        assert_eq!(
            fs::read_to_string("/run/fr-synthetic-ingress").unwrap(),
            "synthetic-only\n"
        );
        assert_ne!(
            fs::read_link("/proc/self/ns/net")
                .unwrap()
                .to_string_lossy(),
            fs::read_to_string("/run/fr-parent-netns").unwrap().trim(),
            "route failure requires the isolated network namespace"
        );
        let output = Command::new("ip")
            .args([
                "-o",
                "route",
                "show",
                "table",
                "local",
                "exact",
                "100.64.0.1/32",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "could not retain original route");
        let original = String::from_utf8(output.stdout).unwrap();
        assert_eq!(original.lines().count(), 1, "unexpected fixture route");
        let original: Vec<String> = original.split_whitespace().map(str::to_owned).collect();
        assert_eq!(original[0], "local");
        assert_eq!(original[1], "100.64.0.1");
        assert!(original.windows(2).any(|pair| pair == ["dev", "lo"]));
        let guard = Self {
            original,
            restored: false,
        };
        let status = Command::new("ip")
            .args([
                "route",
                "replace",
                "unreachable",
                "100.64.0.1/32",
                "table",
                "local",
            ])
            .status()
            .unwrap();
        assert!(status.success(), "could not install fixture route failure");
        guard
    }

    fn restore(&mut self) -> std::io::Result<()> {
        if !self.restored {
            let status = Command::new("ip")
                .args(["route", "replace"])
                .args(&self.original)
                .args(["table", "local"])
                .status()?;
            if !status.success() {
                return Err(std::io::Error::other("could not restore fixture route"));
            }
            self.restored = true;
        }
        Ok(())
    }
}
impl Drop for RouteFailure {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("fixture route cleanup failed: {error}");
        }
    }
}

fn start_reconnect(files: Option<frd::native_files::Directory>, send: &[PathBuf]) -> Controlled {
    Controlled::start_all(
        false,
        false,
        files,
        send,
        None,
        InputSettings {
            reconnect: true,
            ..InputSettings::default()
        },
        None,
    )
}

fn reconnect_viewing(mut s: Controlled) -> Controlled {
    let process = s.client.id();
    let first_maps = s.driver.notices("FOCUSED ").len();
    let listening = s
        .daemon
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| matches!(event, Event::Listening { .. }))
        .count();
    let mut loss = RouteFailure::install();
    // Ordinary ACK/maintenance output now gets a real UDP EHOSTUNREACH. Do
    // not create an input action: its own send/ticket/receipt expiry is terminal
    // and must never be relabeled as a retryable transport interruption.
    assert!(
        eventually(Duration::from_secs(15), || {
            s.alive();
            s.driver.ask("window") == ["WIN", "none"]
        }),
        "old viewer window survived the route failure: {}",
        s.daemon.dump()
    );
    // Restore as soon as independent X11 observation sees old-window cleanup.
    // Waiting for the host listener with the route blocked can consume retries.
    loss.restore().unwrap();
    assert!(
        eventually(Duration::from_secs(20), || {
            s.alive();
            s.daemon
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, Event::Listening { .. }))
                .count()
                > listening
        }),
        "host did not complete original share cleanup: {}",
        s.daemon.dump()
    );
    assert_eq!(
        indicator(&mut s.observer),
        None,
        "old input owner survived cleanup"
    );
    // X11 may reuse an XID. A fresh independent MapNotify plus the supervisor's
    // opened-attempt count proves a new window, not a guessed numeric identity.
    assert!(
        eventually(Duration::from_secs(40), || {
            s.alive();
            s.driver.notices("FOCUSED ").len() > first_maps
        }),
        "same client never opened a fresh viewing window: {}",
        s.daemon.dump()
    );
    s.window = s.viewer_window();
    assert_eq!(s.client.id(), process);
    assert_eq!(
        indicator(&mut s.observer),
        None,
        "reconnect reacquired control"
    );
    let (resting, mask) = host_pointer(&mut s.observer);
    assert_eq!(mask, 0, "old held input was not released");
    let before = host_events(&mut s.observer).len();
    assert_eq!(s.observer.ask("paint 0xb45a31"), ["OK"]);
    assert!(
        eventually(Duration::from_secs(15), || {
            s.alive();
            window_pixels(&s.viewer.display)
                .iter()
                .any(|&(window, pixel)| window == s.window.0 && near(pixel, (0xb4, 0x5a, 0x31)))
        }),
        "fresh viewing did not present changed host pixels"
    );
    s.viewer_move((150, 120));
    s.viewer("tap 0x63");
    s.viewer("button 1 1");
    s.viewer("button 1 0");
    thread::sleep(Duration::from_secs(3));
    s.alive();
    assert_eq!(
        host_pointer(&mut s.observer),
        (resting, 0),
        "viewing-only input moved the host"
    );
    assert_eq!(
        host_events(&mut s.observer).len(),
        before,
        "viewing-only input reached the host"
    );
    s
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; kernel route failure; two Xvfb displays; real input agent"]
fn transient_control_transport_loss_reconnects_fresh_viewing_without_reacquiring_input() {
    finish_viewing_reconnect(reconnect_viewing(start_reconnect(None, &[])));
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; kernel route failure; two Xvfb displays; real input agent"]
fn a_new_native_gesture_requests_fresh_control_after_viewing_reconnect() {
    let (session, upload) = published_upload();
    let mut s = reconnect_viewing(session);
    let process = s.client.id();
    let window = s.window.0;
    let before = host_events(&mut s.observer).len();
    let f8: u32 = s.observer.ask("keycode 0xffc5")[1].parse().unwrap();
    s.viewer("tap 0xffc5");
    assert!(
        s.pointer_follows((180, 140), Duration::from_secs(15)),
        "the explicit fresh request never gained control: {}",
        s.daemon.dump()
    );
    assert!(indicator(&mut s.observer).is_some());
    assert_eq!(s.client.id(), process);
    assert_eq!(
        s.viewer_window().0,
        window,
        "request replaced the viewing window"
    );
    assert!(
        !host_events(&mut s.observer)[before..]
            .iter()
            .any(|event| event.1 == f8),
        "the local control shortcut leaked into the fresh lease"
    );
    let key: u32 = s.observer.ask("keycode 0x64")[1].parse().unwrap();
    s.viewer("tap 0x64");
    assert!(
        eventually(Duration::from_secs(10), || {
            host_events(&mut s.observer)[before..]
                .iter()
                .any(|event| (event.0, event.1, event.3) == (2, key, false))
        }),
        "newly typed input did not reach the host"
    );

    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["last_attempt_mode"], "explicit_control", "{report}");
    assert_eq!(report["control_request_attempt"], 1, "{report}");
    assert_eq!(
        report["control_attempt_end"], "native_session_failed",
        "{report}"
    );
    let fresh = report["explicit_control_requests"].as_array().unwrap();
    assert_eq!(fresh.len(), 1, "{report}");
    assert!(fresh[0]["attempt"].as_u64().unwrap() >= 2, "{report}");
    assert_eq!(fresh[0]["granted"], true, "{report}");
    assert!(
        fresh[0]["input_submitted_to_os"].as_u64().unwrap() > 0,
        "{report}"
    );
    assert_eq!(report["clipboard_requested"], false, "{report}");
    upload.assert_retained(&report);
    assert_eq!(report["cleanup_confirmed"], true, "{report}");
    s.daemon.finish();
}

/// Keep the selected source and destination owned across both connections.
struct SelectedUpload {
    _source: Dir,
    destination: Dir,
    bytes: Vec<u8>,
}
fn published_upload() -> (Controlled, SelectedUpload) {
    let source = Dir::new("recontrol-source");
    let destination = Dir::new("recontrol-drop");
    let path = source.0.join("once.bin");
    let bytes = b"selected before the original control request\n".repeat(1024);
    fs::write(&path, &bytes).unwrap();
    let mut session = start_reconnect(Some(destination.directory()), std::slice::from_ref(&path));
    assert!(
        eventually(Duration::from_secs(30), || {
            session.alive();
            fs::read(destination.0.join("once.bin")).ok().as_ref() == Some(&bytes)
        }),
        "original upload never published its exact bytes: {}",
        session.daemon.dump()
    );
    // Allow the original publication receipt to return before the route loss.
    // The final report must retain that durable receipt, not invent it from
    // the independent filesystem observation above.
    thread::sleep(Duration::from_secs(2));
    // Reusing the original source is now invalid even if a later owner tries.
    fs::write(&path, b"the old selection changed after publication").unwrap();
    (
        session,
        SelectedUpload {
            _source: source,
            destination,
            bytes,
        },
    )
}
impl SelectedUpload {
    fn assert_retained(&self, report: &serde_json::Value) {
        assert_eq!(report["files_requested"], 1, "{report}");
        assert_eq!(
            report["files_sent"],
            serde_json::json!([{"index": 0, "bytes": self.bytes.len(), "durable": true}]),
            "{report}"
        );
        assert_eq!(report["files_refused"], serde_json::json!([]), "{report}");
        assert_eq!(report["files_absence"], serde_json::Value::Null, "{report}");
        assert_eq!(self.destination.entries(), ["once.bin"]);
        assert_eq!(
            fs::read(self.destination.0.join("once.bin")).unwrap(),
            self.bytes
        );
    }
}

fn finish_viewing_reconnect(mut s: Controlled) {
    assert_eq!(
        indicator(&mut s.observer),
        None,
        "viewing-only attempt acquired an executor"
    );
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert!(report["attempts"].as_u64().unwrap() >= 2, "{report}");
    assert!(report["opened"].as_u64().unwrap() >= 2, "{report}");
    assert_eq!(report["last_attempt_mode"], "view_only", "{report}");
    assert!(
        report["viewing_reconnects"].as_u64().unwrap() >= 1,
        "{report}"
    );
    assert_eq!(report["control_request_attempt"], 1, "{report}");
    assert_eq!(report["control_requested"], true, "{report}");
    assert_eq!(report["control_granted"], true, "{report}"); // Historical first grant.
    assert_eq!(
        report["control_attempt_end"], "native_session_failed",
        "{report}"
    );
    assert!(
        report["last_attempt_media"]["decoded"].as_u64().unwrap() > 0,
        "{report}"
    );
    assert_eq!(report["cleanup_confirmed"], true, "{report}");
    s.daemon.finish();
}
