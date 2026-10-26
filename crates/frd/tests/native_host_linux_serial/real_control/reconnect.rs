//! The same shipped client and identity through a kernel route failure.
//! Tailnet admission and displays are the existing namespace fixtures.
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

#[test]
#[ignore = "explicit isolated user/mount/network namespace; kernel route failure; two Xvfb displays; real input agent"]
fn transient_control_transport_loss_reconnects_fresh_viewing_without_reacquiring_input() {
    let mut s = Controlled::start_all(
        false,
        false,
        None,
        &[],
        None,
        InputSettings {
            reconnect: true,
            ..InputSettings::default()
        },
        None,
    );
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
    finish_viewing_reconnect(s);
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
