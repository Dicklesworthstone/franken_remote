use super::*;
use bus::fixture::{Data, Event, Peer, SERIAL, selection, uid};
use std::{sync::atomic::AtomicUsize, task::Wake, time::Instant};
fn wait(mut ready: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(4);
    while !ready() {
        assert!(Instant::now() < until, "logind test deadline");
        thread::sleep(Duration::from_millis(2));
    }
}
fn start(peer: &Peer) -> Watch {
    Watch::spawn(selection(), peer.address.clone(), uid()).unwrap()
}
fn active(w: &Watch) {
    wait(|| w.control.status() != Status::Opening);
    assert_eq!(w.control.status(), Status::Active);
}
fn finish(w: &mut Watch) {
    w.stop();
    wait(|| w.try_finish().unwrap());
}
fn stopped(w: &Watch, expected: StopReason) {
    wait(|| matches!(w.control.status(), Status::Stopped(_)));
    assert_eq!(w.control.status(), Status::Stopped(expected));
}
#[test]
fn real_private_bus_positive_evidence_and_explicit_retirement() {
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data::default());
    let mut w = start(&peer);
    active(&w);
    // Multiple snapshots renew the same original identity without a new grant.
    thread::sleep(Duration::from_millis(650));
    assert_eq!(w.control.status(), Status::Active);
    assert!(matches!(
        Watch::spawn(selection(), peer.address.clone(), uid()),
        Err(Error::Busy)
    ));
    let control = w.control();
    finish(&mut w);
    assert_eq!(control.status(), Status::Stopped(StopReason::OwnerStopped));
}
#[test]
fn missing_negative_and_unsupported_initial_evidence_never_becomes_active() {
    let _serial = SERIAL.lock().unwrap();
    for (data, expected) in [
        (
            Data {
                locked: true,
                ..Data::default()
            },
            StopReason::Locked,
        ),
        (
            Data {
                active: false,
                ..Data::default()
            },
            StopReason::Inactive,
        ),
        (
            Data {
                remote: true,
                ..Data::default()
            },
            StopReason::UnsupportedSession,
        ),
        (
            Data {
                can_lock: false,
                ..Data::default()
            },
            StopReason::UnsupportedSession,
        ),
        (
            Data {
                uid: 1234,
                ..Data::default()
            },
            StopReason::IdentityChanged,
        ),
        (
            Data {
                suspend: true,
                ..Data::default()
            },
            StopReason::Suspending,
        ),
        (
            Data {
                omit: Some("LockedHint"),
                ..Data::default()
            },
            StopReason::Malformed,
        ),
    ] {
        let peer = Peer::new(data);
        let mut w = start(&peer);
        stopped(&w, expected);
        finish(&mut w);
    }
}
#[test]
fn lock_unlock_between_snapshots_is_terminal_not_coalesced_away() {
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data::default());
    let mut w = start(&peer);
    active(&w);
    peer.emit(Event::LockUnlock);
    stopped(&w, StopReason::Locked);
    assert!(!peer.data.lock().unwrap().locked);
    finish(&mut w);
    assert_eq!(w.control.status(), Status::Stopped(StopReason::Locked));
}
#[test]
fn signal_during_initial_snapshot_cannot_publish_late_positive_evidence() {
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data {
        initial_signal: true,
        ..Data::default()
    });
    let mut w = start(&peer);
    stopped(&w, StopReason::Locked);
    finish(&mut w);
}
#[test]
fn lifecycle_signals_and_invalidations_end_the_exact_original_session() {
    let _serial = SERIAL.lock().unwrap();
    for (event, expected) in [
        (Event::Inactive, StopReason::Inactive),
        (Event::Suspend, StopReason::Suspending),
        (Event::Removed, StopReason::SessionUnavailable),
        (Event::OwnerLost, StopReason::IdentityChanged),
        (Event::Invalidated, StopReason::Locked),
    ] {
        let peer = Peer::new(Data::default());
        let mut w = start(&peer);
        active(&w);
        peer.emit(event);
        stopped(&w, expected);
        finish(&mut w);
    }
}
#[test]
fn different_session_events_do_not_end_selected_desktop() {
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data::default());
    let mut w = start(&peer);
    active(&w);
    peer.emit(Event::Unrelated);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(w.control.status(), Status::Active);
    finish(&mut w);
}
#[test]
fn reused_session_name_does_not_revalidate_changed_creation_identity() {
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data::default());
    let mut w = start(&peer);
    active(&w);
    peer.data.lock().unwrap().stamp += 1;
    stopped(&w, StopReason::IdentityChanged);
    finish(&mut w);
}
#[test]
fn unavailable_or_untrusted_bus_fails_closed() {
    let _serial = SERIAL.lock().unwrap();
    let mut w = Watch::spawn(
        selection(),
        "unix:path=/nonexistent/fr-test-bus".into(),
        uid(),
    )
    .unwrap();
    stopped(&w, StopReason::BusUnavailable);
    finish(&mut w);
    let peer = Peer::new(Data::default());
    let mut w = Watch::spawn(selection(), peer.address.clone(), uid() + 1).unwrap();
    stopped(&w, StopReason::UntrustedService);
    finish(&mut w);
}
#[test]
fn missing_method_reply_cannot_extend_evidence_forever() {
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data::default());
    let mut w = start(&peer);
    active(&w);
    peer.data.lock().unwrap().no_reply = true;
    wait(|| matches!(w.control.status(), Status::Stopped(_)));
    assert!(matches!(
        w.control.status(),
        Status::Stopped(StopReason::BusUnavailable | StopReason::EvidenceExpired)
    ));
    finish(&mut w);
}
struct Notified(AtomicUsize);
impl Wake for Notified {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
#[test]
fn terminal_native_event_wakes_consumer_without_authority_lock() {
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data::default());
    let mut w = start(&peer);
    active(&w);
    let notified = Arc::new(Notified(AtomicUsize::new(0)));
    let wake = Waker::from(notified.clone());
    let task = Context::from_waker(&wake);
    w.control.register(&task);
    peer.emit(Event::LockUnlock);
    stopped(&w, StopReason::Locked);
    wait(|| notified.0.load(Ordering::Acquire) != 0);
    finish(&mut w);
}
#[test]
fn boottime_expiry_is_terminal_even_without_native_progress() {
    let shared = Shared {
        state: AtomicU8::new(1),
        deadline: AtomicU64::new(50),
        waker: Mutex::new(None),
    };
    assert_eq!(shared.status(Ok(49)), Status::Active);
    assert_eq!(
        shared.status(Ok(50)),
        Status::Stopped(StopReason::EvidenceExpired)
    );
    assert!(shared.publish(bus::boottime().unwrap()).is_err());
    assert_eq!(
        shared.status(Ok(0)),
        Status::Stopped(StopReason::EvidenceExpired)
    );
}
#[test]
fn invalid_selection_and_sensitive_debug() {
    for bad in ["", "../session", "a'b", "session\n", "a:b"] {
        let mut s = selection();
        s.session = bad.into();
        assert_eq!(s.validate(), Err(Error::Selection));
    }
    let mut s = selection();
    s.display = "host:0".into();
    assert_eq!(s.validate(), Err(Error::Selection));
    assert!(!format!("{s:?}").contains("host:0"));
}

#[test]
fn same_uid_unrelated_bus_client_cannot_forge_logind_lock() {
    let _serial = SERIAL.lock().unwrap();
    let peer = Peer::new(Data::default());
    let mut w = start(&peer);
    active(&w);
    peer.spoof_lock();
    thread::sleep(Duration::from_millis(200));
    assert_eq!(w.control.status(), Status::Active);
    finish(&mut w);
}
