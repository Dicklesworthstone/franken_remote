//! Actual parked watcher; no native UI or media evidence is fabricated.
use super::*;
use std::sync::mpsc;

fn control() -> (Control, Arc<AtomicBool>) {
    let stopped = Arc::new(AtomicBool::new(false));
    let original = stopped.clone();
    let shared = Arc::new(Shared {
        on_stop: Box::new(move || original.store(true, Ordering::Release)),
        state: Mutex::new(opening(now_ns().unwrap())),
        changed: Condvar::new(),
        #[cfg(target_os = "linux")]
        watcher_wake: crate::host_indicator::process::watcher::Wake::new().unwrap(),
        native_wake: crate::native_wait::Wake::new().unwrap(),
    });
    (Control(shared), stopped)
}

// Acquiring the state mutex after its first new sample proves the watcher
// completed the check and released that lock for its wait. Linux notifications
// must also survive publication before the kernel wait starts. Tests do not
// rely on a delay hoping the background thread was scheduled.
fn parked(control: &Control, before: u64) {
    let until = Instant::now() + Duration::from_secs(2);
    while control.0.state().last == before {
        assert!(
            Instant::now() < until,
            "watcher never sampled its original owner"
        );
        thread::yield_now();
    }
}

#[test]
fn a_stalled_native_owner_expires_without_another_host_or_worker_check() {
    let (control, stopped) = control();
    // Admit a real original short readiness budget, then never run its owner.
    control.0.ready(now_ns().unwrap()).unwrap();
    let original_until = control.0.state().until;
    assert_eq!(
        control.wait_for_stop(&AtomicBool::new(false)),
        Some(Error::Expired)
    );
    assert!(now_ns().unwrap() >= original_until);
    assert!(stopped.load(Ordering::Acquire));
    assert_eq!(control.0.ready(now_ns().unwrap()), Err(Error::Expired));
}

#[test]
fn first_readiness_shortens_a_parked_opening_without_waiting_three_seconds() {
    let (control, stopped) = control();
    let finished = AtomicBool::new(false);
    let (completed, completion) = mpsc::sync_channel(1);
    let before = control.0.state().last;
    thread::scope(|scope| {
        let watched = &control;
        let finished = &finished;
        scope.spawn(move || {
            completed.send(watched.wait_for_stop(finished)).unwrap();
        });
        parked(&control, before);
        control.0.ready(now_ns().unwrap()).unwrap();
        // The immutable Check budget is 250 ms. An unchanged Opening wait
        // would last three seconds; leave ample scheduling margin here.
        assert_eq!(
            completion.recv_timeout(Duration::from_secs(2)).unwrap(),
            Some(Error::Expired)
        );
    });
    assert!(stopped.load(Ordering::Acquire));
}

#[test]
fn a_renewal_outlives_the_old_wait_without_reviving_expired_evidence() {
    let (control, stopped) = control();
    // A delayed but still valid Check response has already consumed half its
    // original budget. Publication must not manufacture a fresh 250 ms.
    let issued = now_ns().unwrap() - 125_000_000;
    control.0.ready(issued).unwrap();
    let first_until = control.0.state().until;
    let before = control.0.state().last;
    let finished = AtomicBool::new(false);
    let (completed, completion) = mpsc::sync_channel(1);
    thread::scope(|scope| {
        let watched = &control;
        let finished = &finished;
        scope.spawn(move || {
            completed.send(watched.wait_for_stop(finished)).unwrap();
        });
        parked(&control, before);
        control.0.ready(now_ns().unwrap()).unwrap();
        let renewed_until = control.0.state().until;
        assert!(renewed_until >= first_until + 125_000_000);
        assert_eq!(
            completion.recv_timeout(Duration::from_secs(1)).unwrap(),
            Some(Error::Expired)
        );
        assert!(now_ns().unwrap() >= renewed_until);
    });
    assert!(stopped.load(Ordering::Acquire));
    assert_eq!(control.0.ready(now_ns().unwrap()), Err(Error::Expired));
}

#[test]
fn terminal_notifications_are_not_lost_before_or_after_a_wait_starts() {
    for stop_first in [false, true] {
        let (control, stopped) = control();
        let finished = AtomicBool::new(false);
        let (completed, completion) = mpsc::sync_channel(1);
        if stop_first {
            control.0.stop(Error::LocalRevoke);
        }
        let before = control.0.state().last;
        thread::scope(|scope| {
            let watched = &control;
            let finished = &finished;
            scope.spawn(move || {
                completed.send(watched.wait_for_stop(finished)).unwrap();
            });
            if !stop_first {
                parked(&control, before);
                control.0.stop(Error::LocalRevoke);
            }
            assert_eq!(
                completion.recv_timeout(Duration::from_secs(1)).unwrap(),
                Some(Error::LocalRevoke)
            );
        });
        assert!(stopped.load(Ordering::Acquire));
        assert_eq!(control.0.ready(now_ns().unwrap()), Err(Error::LocalRevoke));
    }
}

#[test]
fn completion_interrupts_the_original_wait_without_changing_ui_evidence() {
    for finish_first in [false, true] {
        let (control, stopped) = control();
        let finished = AtomicBool::new(finish_first);
        let (completed, completion) = mpsc::sync_channel(1);
        let before = control.0.state().last;
        thread::scope(|scope| {
            let watched = &control;
            let finished = &finished;
            scope.spawn(move || {
                completed.send(watched.wait_for_stop(finished)).unwrap();
            });
            if !finish_first {
                parked(&control, before);
            }
            finished.store(true, Ordering::Release);
            control.wake_watcher();
            assert_eq!(
                completion.recv_timeout(Duration::from_secs(1)).unwrap(),
                None
            );
        });
        assert_eq!(control.0.state().status, Status::Opening);
        assert!(!stopped.load(Ordering::Acquire));
    }
}

#[test]
fn poisoned_watcher_state_stops_the_host_outside_its_state_lock() {
    let (control, stopped) = control();
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _state = control.0.state.lock().unwrap();
        panic!("failed evidence publisher");
    }));
    assert!(poisoned.is_err());
    assert_eq!(
        control.wait_for_stop(&AtomicBool::new(false)),
        Some(Error::Panicked)
    );
    assert!(stopped.load(Ordering::Acquire));
    assert_eq!(control.0.ready(now_ns().unwrap()), Err(Error::Panicked));
}
