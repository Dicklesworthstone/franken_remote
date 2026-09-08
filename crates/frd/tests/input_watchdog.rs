use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    time::{TimerDriverHandle, VirtualClock},
    types::{Budget, CancelKind, Time},
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_submission::*,
    time::{HostDuration, HostInstant},
};
use frd::input_watchdog::{Control, StopReason, Watchdog, host_now};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};
fn runtime() -> Runtime {
    RuntimeBuilder::new().worker_threads(1).build().unwrap()
}
fn session(cx: &Cx, lifetime: u64) -> InputSession {
    let c = InputCredentials {
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
    let now = host_now(cx).unwrap();
    let mut a = SessionAuthority::new(
        c.session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(lifetime),
            ticket_lifetime: HostDuration::from_micros(lifetime / 4),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(now).unwrap();
    a.mark_view_ready(now).unwrap();
    a.grant_lease(c.lease, now).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, now).unwrap();
    InputSession::new(
        a,
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        now,
    )
    .unwrap()
}
struct WakeCount(std::sync::atomic::AtomicUsize);
impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}
fn count_waker() -> (Arc<WakeCount>, Waker) {
    let n = Arc::new(WakeCount(std::sync::atomic::AtomicUsize::new(0)));
    (n.clone(), Waker::from(n))
}
fn virtual_runtime() -> (Runtime, Arc<VirtualClock>, TimerDriverHandle) {
    let clock = Arc::new(VirtualClock::new());
    let driver = TimerDriverHandle::with_virtual_clock(clock.clone());
    (
        RuntimeBuilder::new()
            .worker_threads(1)
            .with_timer_driver(driver.clone())
            .build()
            .unwrap(),
        clock,
        driver,
    )
}
#[test]
fn actual_runtime_expires_without_any_input_or_renewal_traffic() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let owner = session(&cx, 50_000);
    let watchdog = Watchdog::new(cx, owner.monitor()).unwrap();
    let control = watchdog.control();
    // A failed timer cannot hang the test: this thread is only a failure fuse.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let fuse_control = control.clone();
    let fuse = std::thread::spawn(move || {
        if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
            fuse_control.stop(StopReason::NativeFailure);
        }
    });
    let reason = r.block_on(watchdog);
    let _ = done_tx.send(());
    fuse.join().unwrap();
    assert_eq!(reason, StopReason::AuthorityEnded);
    assert!(control.is_stopped());
}
#[test]
fn ticket_expiry_is_not_lease_expiry_and_renewals_do_not_slide_with_polling() {
    let (r, clock, driver) = virtual_runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let mut owner = session(&cx, 3_000_000);
    let mut watchdog = Watchdog::new(cx, owner.monitor()).unwrap();
    let (_, waker) = count_waker();
    let mut task = Context::from_waker(&waker);
    clock.advance_to(Time::from_millis(1000));
    assert_eq!(Pin::new(&mut watchdog).poll(&mut task), Poll::Pending);
    owner
        .issue_observation_challenge(17, HostInstant::from_micros(1_000_000))
        .unwrap();
    owner
        .renew_observation(17, HostInstant::from_micros(1_100_000))
        .unwrap();
    owner
        .issue_control_challenge(18, HostInstant::from_micros(1_100_000))
        .unwrap();
    owner
        .renew_control(18, HostInstant::from_micros(1_200_000))
        .unwrap();
    clock.advance_to(Time::from_millis(3000));
    let _ = driver.process_timers();
    assert_eq!(Pin::new(&mut watchdog).poll(&mut task), Poll::Pending);
    clock.advance_to(Time::from_millis(4000));
    let _ = driver.process_timers();
    assert_eq!(
        Pin::new(&mut watchdog).poll(&mut task),
        Poll::Ready(StopReason::AuthorityEnded)
    );
    assert!(
        owner
            .issue_ticket(
                InputTicketId::from_raw(5),
                HostInstant::from_micros(4_000_000)
            )
            .is_err()
    );
}
#[test]
fn local_revoke_fences_synchronously_wakes_and_keeps_first_reason() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let owner = session(&cx, 3_000_000);
    let mut watchdog = Watchdog::new(cx, owner.monitor()).unwrap();
    let control = watchdog.control();
    let (count, waker) = count_waker();
    let mut task = Context::from_waker(&waker);
    assert_eq!(Pin::new(&mut watchdog).poll(&mut task), Poll::Pending);
    control.stop(StopReason::LocalRevoke);
    assert!(owner.monitor().is_revoked());
    assert!(count.0.load(std::sync::atomic::Ordering::Relaxed) > 0);
    control.stop(StopReason::ClientDisconnected);
    assert_eq!(
        Pin::new(&mut watchdog).poll(&mut task),
        Poll::Ready(StopReason::LocalRevoke)
    );
}
#[test]
fn drop_even_before_first_poll_revokes_and_ancient_handles_cannot_rebind() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let owner = session(&cx, 3_000_000);
    let watchdog = Watchdog::new(cx.clone(), owner.monitor()).unwrap();
    let old = watchdog.control();
    drop(watchdog);
    assert_eq!(old.reason(), Some(StopReason::WatchdogDropped));
    assert!(owner.monitor().is_revoked());
    let other = session(&cx, 3_000_000);
    old.stop(StopReason::LocalRevoke);
    assert!(!other.monitor().is_revoked());
}
#[test]
fn cancellation_is_terminal_and_cannot_be_renewed_away() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let mut owner = session(&cx, 3_000_000);
    let mut watchdog = Watchdog::new(cx.clone(), owner.monitor()).unwrap();
    let (_, waker) = count_waker();
    let mut task = Context::from_waker(&waker);
    assert_eq!(Pin::new(&mut watchdog).poll(&mut task), Poll::Pending);
    cx.cancel_fast(CancelKind::User);
    assert_eq!(
        Pin::new(&mut watchdog).poll(&mut task),
        Poll::Ready(StopReason::Cancelled)
    );
    assert!(
        owner
            .issue_ticket(InputTicketId::from_raw(4), host_now(&cx).unwrap())
            .is_err()
    );
}
#[test]
fn clock_regression_fences_and_spurious_polls_retain_only_one_timer() {
    let (r, clock, driver) = virtual_runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let owner = session(&cx, 3_000_000);
    let before = driver.pending_count();
    let mut watchdog = Watchdog::new(cx, owner.monitor()).unwrap();
    let (_, waker) = count_waker();
    let mut task = Context::from_waker(&waker);
    for _ in 0..100 {
        assert_eq!(Pin::new(&mut watchdog).poll(&mut task), Poll::Pending);
    }
    assert_eq!(driver.pending_count(), before + 1);
    clock.advance_to(Time::from_millis(1));
    assert_eq!(Pin::new(&mut watchdog).poll(&mut task), Poll::Pending);
    clock.set(Time::ZERO);
    assert_eq!(
        Pin::new(&mut watchdog).poll(&mut task),
        Poll::Ready(StopReason::ClockRegression)
    );
    assert_eq!(driver.pending_count(), before);
}
#[test]
fn dropping_a_polled_watchdog_cancels_its_timer() {
    let (r, _, driver) = virtual_runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let owner = session(&cx, 3_000_000);
    let before = driver.pending_count();
    let mut watchdog = Watchdog::new(cx, owner.monitor()).unwrap();
    let (_, waker) = count_waker();
    let mut task = Context::from_waker(&waker);
    assert_eq!(Pin::new(&mut watchdog).poll(&mut task), Poll::Pending);
    drop(watchdog);
    assert_eq!(driver.pending_count(), before);
    assert!(owner.monitor().is_revoked());
}
#[test]
fn another_runtime_ambient_clock_cannot_take_over_the_timer() {
    let (a, clock, driver) = virtual_runtime();
    let cx = a.request_cx_with_budget(Budget::INFINITE);
    let owner = session(&cx, 3_000_000);
    let mut watchdog = Watchdog::new(cx, owner.monitor()).unwrap();
    let other = runtime();
    other.block_on(async {
        std::future::poll_fn(|task| {
            assert_eq!(Pin::new(&mut watchdog).poll(task), Poll::Pending);
            Poll::Ready(())
        })
        .await;
    });
    clock.advance_to(Time::from_secs(3));
    assert_eq!(driver.process_timers(), 1);
    let (_, waker) = count_waker();
    let mut task = Context::from_waker(&waker);
    assert_eq!(
        Pin::new(&mut watchdog).poll(&mut task),
        Poll::Ready(StopReason::AuthorityEnded)
    );
}
fn control_is_send_sync(_: &impl Send, _: &impl Sync) {}
#[test]
fn control_is_content_free_and_thread_safe() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let owner = session(&cx, 3_000_000);
    let w = Watchdog::new(cx, owner.monitor()).unwrap();
    let c: Control = w.control();
    control_is_send_sync(&c, &c);
    assert_eq!(
        format!("{c:?}"),
        "InputControl { stopped: false, reason: None, .. }"
    );
}
