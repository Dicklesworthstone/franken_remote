//! Force contention on the real stop path without a blocking executor callback.
use super::*;
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{Capabilities, InputSession},
    time::HostDuration,
};
use std::{
    sync::atomic::AtomicUsize,
    task::Wake,
    thread,
    time::{Duration, Instant},
};

fn watchdog(runtime: &Runtime) -> (Watchdog, InputSession) {
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let now = host_now(&cx).unwrap();
    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut authority = SessionAuthority::new(
        credentials.session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(3_000_000),
            ticket_lifetime: HostDuration::from_micros(1_000_000),
        },
    );
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    authority.mark_view_ready(now).unwrap();
    authority.grant_lease(credentials.lease, now).unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, now)
        .unwrap();
    let input = InputSession::new(
        authority,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        now,
    )
    .unwrap();
    (Watchdog::new(cx, input.monitor()).unwrap(), input)
}

#[derive(Default)]
struct Wakes(AtomicUsize);
impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

// Wall time is only a deadlock/failure fuse. Test progress is selected by the
// real monitor and watchdog registration, never by a guessed scheduling delay.
fn wait_for(mut ready: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while !ready() {
        if start.elapsed() >= Duration::from_secs(2) {
            return false;
        }
        thread::yield_now();
    }
    true
}

#[test]
fn typed_stop_is_visible_before_a_contended_executor_fence_can_run() {
    for reason in [
        StopReason::GeometryChanged,
        StopReason::Suspended,
        StopReason::LocalRevoke,
    ] {
        let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
        let (mut watchdog, input) = watchdog(&runtime);
        let control = watchdog.control();
        let wakes = Arc::new(Wakes::default());
        let fences = Arc::new(AtomicUsize::new(0));
        let monitor = input.monitor();
        let calls = fences.clone();
        assert!(control.install_fence(Box::new(move || {
            assert!(monitor.is_revoked(), "authority must be fenced first");
            calls.fetch_add(1, Ordering::SeqCst);
        })));

        // Hold the actual executor mutex before the original stop. It must
        // publish both its cause and authority fence without taking this lock.
        let held = control.0.fence.lock().unwrap();
        let original = control.clone();
        let stopper = thread::spawn(move || original.stop(reason));
        let revoked = wait_for(|| control.is_stopped());
        let original_cause = control.reason();

        // Poll the actual independent watchdog while the original stop is
        // still blocked. Previously it saw no cause and selected AuthorityEnded.
        let observer_wakes = wakes.clone();
        let observer = thread::spawn(move || {
            let waker = Waker::from(observer_wakes);
            Pin::new(&mut watchdog).poll(&mut Context::from_waker(&waker))
        });
        let registered = wait_for(|| control.0.waker.lock().unwrap().is_some());
        let contended_cause = control.reason();
        let early_fences = fences.load(Ordering::SeqCst);
        let early_wakes = wakes.0.load(Ordering::SeqCst);

        // Release before joining or asserting, including on a failed fuse, so
        // a planted old implementation fails without leaving blocked threads.
        drop(held);
        stopper.join().unwrap();
        let reported = observer.join().unwrap();
        assert!(revoked, "executor contention must not delay revocation");
        assert!(registered, "independent watchdog was not polled");
        assert_eq!(original_cause, Some(reason));
        assert_eq!(contended_cause, Some(reason));
        assert_eq!(early_fences, 0);
        assert_eq!(early_wakes, 0);
        assert_eq!(reported, Poll::Ready(reason));
        assert_eq!(fences.load(Ordering::SeqCst), 2);
        assert_eq!(wakes.0.load(Ordering::SeqCst), 1);

        // Repeated stops still execute the executor fence, but cannot replace
        // the cause that controls geometry recovery and terminal reporting.
        control.stop(StopReason::AuthorityEnded);
        assert_eq!(control.reason(), Some(reason));
        assert_eq!(fences.load(Ordering::SeqCst), 3);
    }
}

#[test]
fn direct_monitor_revocation_still_records_authority_ended() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let (mut watchdog, input) = watchdog(&runtime);
    let control = watchdog.control();
    input.monitor().revoke();
    assert!(control.is_stopped());
    assert_eq!(control.reason(), None);
    let waker = Waker::from(Arc::new(Wakes::default()));
    assert_eq!(
        Pin::new(&mut watchdog).poll(&mut Context::from_waker(&waker)),
        Poll::Ready(StopReason::AuthorityEnded)
    );
    assert_eq!(control.reason(), Some(StopReason::AuthorityEnded));
}
