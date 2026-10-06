use super::*;
use std::sync::atomic::AtomicBool;

fn fixture(mapped: bool, last: Instant) -> (IndicatorControl, Arc<AtomicBool>) {
    let revoked = Arc::new(AtomicBool::new(false));
    let flag = revoked.clone();
    let control = IndicatorControl(Arc::new(Shared {
        owner: Owner::Local(Box::new(move || {
            flag.store(true, Ordering::Release);
        })),
        state: AtomicU8::new(u8::from(mapped)),
        window: AtomicU32::new(0),
        last_turn: Mutex::new(last),
    }));
    (control, revoked)
}

#[test]
fn mapping_and_native_progress_are_independent_prerequisites() {
    let (opening, revoked) = fixture(false, Instant::now());
    assert!(!opening.responsive());
    assert!(!revoked.load(Ordering::Acquire));
    let (mapped, revoked) = fixture(true, Instant::now());
    assert!(mapped.responsive());
    assert!(!revoked.load(Ordering::Acquire));
}

#[test]
fn native_worker_stall_revokes_before_a_late_turn_can_refresh_it() {
    let old = Instant::now().checked_sub(PROGRESS_TIMEOUT).unwrap();
    let (control, revoked) = fixture(true, old);
    assert!(!control.responsive());
    assert!(revoked.load(Ordering::Acquire));
    assert_eq!(control.status(), Status::Stopped(StopReason::NativeFailure));
    *control.0.last_turn.lock().unwrap() = Instant::now();
    assert!(!control.responsive());
    assert_eq!(control.status(), Status::Stopped(StopReason::NativeFailure));
}

#[test]
fn a_local_stop_cannot_be_overwritten_by_recent_native_progress() {
    let (control, revoked) = fixture(true, Instant::now());
    control.stop();
    assert!(revoked.load(Ordering::Acquire));
    *control.0.last_turn.lock().unwrap() = Instant::now();
    assert!(!control.responsive());
    assert_eq!(control.status(), Status::Stopped(StopReason::User));
}
