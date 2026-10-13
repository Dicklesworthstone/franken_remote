//! Re-check cadence of the scoped native session. Timer fixtures only, not
//! protocol, tailnet or ingress qualification.
use super::*;
use asupersync::{runtime::RuntimeBuilder, time::sleep, types::Budget};
use std::sync::atomic::AtomicUsize;

#[test]
fn idle_acceptance_rechecks_rarely_and_an_accepted_peer_on_the_short_pulse() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    let session = runtime.request_cx_with_budget(Budget::INFINITE);
    let checks = Arc::new(AtomicUsize::new(0));
    let counted = checks.clone();
    let at_accept = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::new(AtomicBool::new(false));
    let (flag, seen, clock) = (accepted.clone(), at_accept.clone(), session.clone());
    let scoped = Scoped {
        cx: session.clone(),
        check: Arc::new(move || {
            counted.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }),
        pulse: Box::pin(sleep(session.now(), IDLE_PULSE)),
        accepted,
        admitted_pulse: false,
        inner: Box::pin(async move {
            // 300 ms of idle acceptance, then a peer is accepted and parks.
            sleep(clock.now(), Duration::from_millis(300)).await;
            seen.store(checks_now(&checks), Ordering::Relaxed);
            flag.store(true, Ordering::Release);
            sleep(clock.now(), Duration::from_millis(300)).await;
            Ok::<_, Error>(checks_now(&checks))
        }),
    };
    let total = runtime.block_on(scoped).unwrap();
    let idle = at_accept.load(Ordering::Relaxed);
    // Idle: the first poll, at most two 250 ms pulses and the fixture's own
    // timer, two checks each. A 10 ms pulse would be about 60.
    assert!(
        idle <= 10,
        "idle acceptance re-checked {idle} times in 300 ms"
    );
    // Accepted: about 30 short pulses in 300 ms, two checks each; this floor
    // still holds with only half of them under load.
    assert!(
        total - idle >= 30,
        "an accepted peer was re-checked {} times in 300 ms",
        total - idle
    );
}
fn checks_now(checks: &AtomicUsize) -> usize {
    checks.load(Ordering::Relaxed)
}
