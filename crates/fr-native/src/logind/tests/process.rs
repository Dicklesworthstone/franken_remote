//! Actual monitor process and authenticated D-Bus messages. Only login1's data
//! is synthetic. The installed socket is bind-mounted inside a private namespace.
use super::bus::fixture::{Data, Event, Peer, SERIAL};
use frd::session_monitor::{Configuration, Error, Monitor, Selection, State, Status};
use std::{
    process::Command,
    thread,
    time::{Duration, Instant},
};

struct InstalledBus;
impl InstalledBus {
    fn mount(peer: &Peer) -> Self {
        let outer =
            std::env::var("FR_MONITOR_OUTER_MNT").expect("use the private namespace runner");
        assert_ne!(
            std::fs::read_link("/proc/self/ns/mnt")
                .unwrap()
                .to_string_lossy(),
            outer
        );
        assert_eq!(
            std::fs::read_to_string("/run/fr-monitor-private").unwrap(),
            "private\n"
        );
        let path = peer
            .address
            .strip_prefix("unix:path=")
            .unwrap()
            .split(',')
            .next()
            .unwrap();
        assert!(
            Command::new("mount")
                .args(["--bind", path, "/run/dbus/system_bus_socket"])
                .status()
                .unwrap()
                .success()
        );
        Self
    }
}
impl Drop for InstalledBus {
    fn drop(&mut self) {
        assert!(
            Command::new("umount")
                .arg("/run/dbus/system_bus_socket")
                .status()
                .unwrap()
                .success()
        );
    }
}
fn configuration() -> Configuration {
    let image = std::env::current_exe()
        .unwrap()
        .ancestors()
        .map(|p| p.join("fr-session-monitor"))
        .find(|p| p.is_file())
        .unwrap();
    Configuration {
        image,
        selection: Selection {
            session: "c1".into(),
            uid: 1000,
            seat: "seat0".into(),
            display: ":7".into(),
        },
    }
}
fn wait(mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(4);
    while !f() {
        assert!(Instant::now() < until, "monitor process timeout");
        thread::sleep(Duration::from_millis(2));
    }
}
fn active(m: &Monitor) {
    wait(|| m.control().status() != Status::Opening);
    assert_eq!(m.control().status(), Status::Active);
}
fn reaped(m: &mut Monitor) {
    m.stop();
    wait(|| {
        m.try_finish().is_some_and(|result| {
            assert_eq!(result, Ok(()));
            true
        })
    });
}

#[test]
#[ignore = "private user/mount namespace and fixture installed D-Bus socket"]
fn shipped_monitor_retains_lock_logout_and_suspend_signals_across_the_process_boundary() {
    let _serial = SERIAL.lock().unwrap();
    for (event, state) in [
        (Event::LockUnlock, State::Locked),
        (Event::Inactive, State::Inactive),
        (Event::Suspend, State::Suspending),
        (Event::Removed, State::SessionUnavailable),
    ] {
        let peer = Peer::new(Data::default());
        let _bus = InstalledBus::mount(&peer);
        let mut monitor = Monitor::start(configuration()).unwrap();
        active(&monitor);
        thread::sleep(Duration::from_millis(650));
        assert_eq!(
            monitor.control().status(),
            Status::Active,
            "native renewal, not a fabricated active flag"
        );
        peer.emit(event);
        wait(|| matches!(monitor.control().status(), Status::Stopped(_)));
        assert_eq!(
            monitor.control().status(),
            Status::Stopped(Error::Native(state))
        );
        reaped(&mut monitor);
        assert_eq!(
            monitor.control().status(),
            Status::Stopped(Error::Native(state)),
            "cleanup must retain the original cause"
        );
    }
}

#[test]
#[ignore = "private user/mount namespace and fixture installed D-Bus socket"]
fn shipped_monitor_never_admits_locked_wrong_identity_or_unresponsive_sessions() {
    let _serial = SERIAL.lock().unwrap();
    for data in [
        Data {
            locked: true,
            ..Data::default()
        },
        Data {
            uid: 1001,
            ..Data::default()
        },
        Data {
            no_reply: true,
            ..Data::default()
        },
    ] {
        let peer = Peer::new(data);
        let _bus = InstalledBus::mount(&peer);
        let mut monitor = Monitor::start(configuration()).unwrap();
        wait(|| monitor.control().status() != Status::Opening);
        assert!(matches!(monitor.control().status(), Status::Stopped(_)));
        reaped(&mut monitor);
    }
}

#[test]
#[ignore = "private user/mount namespace and fixture installed D-Bus socket"]
fn shipped_monitor_stops_when_login1_evidence_stalls_even_though_ipc_still_answers() {
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data::default());
    let _bus = InstalledBus::mount(&peer);
    let mut monitor = Monitor::start(configuration()).unwrap();
    active(&monitor);
    let started = Instant::now();
    peer.data.lock().unwrap().no_reply = true;
    wait(|| matches!(monitor.control().status(), Status::Stopped(_)));
    assert!(started.elapsed() < Duration::from_secs(1));
    // Depending on which original deadline wins, parent expiry or the native
    // read's terminal error is retained. Neither is a new positive observation.
    assert_ne!(monitor.control().status(), Status::Active);
    reaped(&mut monitor);
    peer.data.lock().unwrap().no_reply = false;
    assert!(
        matches!(monitor.control().status(), Status::Stopped(_)),
        "no resurrection after repair"
    );
}
