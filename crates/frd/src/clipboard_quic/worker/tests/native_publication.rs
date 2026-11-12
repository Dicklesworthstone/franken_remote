//! The production native guard retains its original authorization across a
//! delayed policy read. Channel handoffs advance the real virtual timer without
//! sleeps; the platform operation remains deliberately outside these checks.
use super::*;
use fr_wire::clipboard::session::egress::Transport;
use std::sync::{
    atomic::{AtomicU64, AtomicUsize},
    mpsc,
};

fn native_runtime() -> (Arc<VirtualClock>, Runtime, Cx) {
    let clock = Arc::new(VirtualClock::new());
    let runtime = RuntimeBuilder::new()
        .worker_threads(1)
        .with_timer_driver(TimerDriverHandle::with_virtual_clock(clock.clone()))
        .build()
        .unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    (clock, runtime, cx)
}

fn native_seed(session: Session, gate: Transport, clock: Clock, cx: Cx) -> WorkerSeed {
    WorkerSeed {
        session: Some(session),
        shared: Arc::new(Shared {
            gate,
            maximum: ProtocolLimits::ABSOLUTE.max_control_message_bytes() as usize,
            inbox: Mutex::default(),
            outbox: Mutex::default(),
            inbound_until: AtomicU64::default(),
            inbound_bound: AtomicU64::default(),
            wake: std::sync::OnceLock::default(),
        }),
        clock,
        cx,
    }
}

struct PolicyRead {
    entered: mpsc::SyncSender<HostInstant>,
    release: mpsc::Receiver<()>,
}
struct DeferredPolicy {
    until: AtomicU64,
    revoked: AtomicBool,
    calls: AtomicUsize,
    next: Mutex<Option<PolicyRead>>,
}
struct DeferredOwner(Arc<DeferredPolicy>);
impl Authority for DeferredOwner {
    fn binding(&self) -> Binding {
        binding()
    }
    fn is_revoked(&self) -> bool {
        self.0.revoked.load(Ordering::Acquire)
    }
    fn deadline(&self, now: HostInstant) -> Result<HostInstant, Refusal> {
        self.0.calls.fetch_add(1, Ordering::AcqRel);
        if self.is_revoked() {
            return Err(Refusal::Revoked);
        }
        let until = HostInstant::from_micros(self.0.until.load(Ordering::Acquire));
        if now >= until {
            return Err(Refusal::Authority(AuthorityError::LeaseExpired));
        }
        let next = self.0.next.lock().unwrap().take();
        if let Some(turn) = next {
            turn.entered
                .send(now)
                .map_err(|_| Refusal::AuthorityUnavailable)?;
            turn.release
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| Refusal::AuthorityUnavailable)?;
        }
        // The default publication_refusal is deliberately only a terminal
        // atomic fence. It cannot replace this originally authorized cap.
        Ok(until)
    }
    fn revoke(&self) {
        self.0.revoked.store(true, Ordering::Release);
    }
}
struct HostBoundary {
    clock: Arc<VirtualClock>,
    policy: Arc<DeferredPolicy>,
    access: Option<NativeAccess>,
    seed: WorkerSeed,
    _runtime: Runtime,
}
impl HostBoundary {
    fn new(until: u64) -> Self {
        let (clock, runtime, cx) = native_runtime();
        let policy = Arc::new(DeferredPolicy {
            until: AtomicU64::new(until),
            revoked: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
            next: Mutex::default(),
        });
        let channel = ChannelSession::with_monitor(
            Monitor::new(DeferredOwner(policy.clone())),
            context(),
            ProtocolLimits::ABSOLUTE,
            true,
            HostInstant::ORIGIN,
        )
        .unwrap();
        let gate = channel.transport();
        let seed = native_seed(Session::Host(channel), gate.clone(), Clock::Host(gate), cx);
        let access = seed.native_access();
        Self {
            clock,
            policy,
            access: Some(access),
            seed,
            _runtime: runtime,
        }
    }
    fn during_policy_read(
        &mut self,
        until: Option<HostInstant>,
        change: impl FnOnce(&Self),
    ) -> bool {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        *self.policy.next.lock().unwrap() = Some(PolicyRead {
            entered: entered_tx,
            release: release_rx,
        });
        self.policy.calls.store(0, Ordering::Release);
        let access = self.access.take().unwrap();
        let worker = thread::spawn(move || {
            let allowed = access.check(until);
            (access, allowed)
        });
        let entered = entered_rx.recv_timeout(Duration::from_secs(5));
        if entered.is_ok() {
            change(self);
        }
        let _ = release_tx.send(());
        let (access, allowed) = worker.join().unwrap();
        self.access = Some(access);
        assert_eq!(entered.unwrap(), HostInstant::ORIGIN);
        // The final check must not authorize again under a replacement cap.
        assert_eq!(self.policy.calls.load(Ordering::Acquire), 1);
        allowed
    }
    fn check(&self, until: Option<HostInstant>) -> bool {
        self.access.as_ref().unwrap().check(until)
    }
}

#[test]
fn native_policy_wait_cannot_outlive_the_original_authorization() {
    for item in [None, Some(HostInstant::from_micros(9_000))] {
        let mut boundary = HostBoundary::new(1_000);
        assert!(!boundary.during_policy_read(item, |boundary| {
            boundary.clock.advance_to(Time::from_millis(1));
        }));
        assert!(!boundary.policy.revoked.load(Ordering::Acquire));
        assert!(boundary.seed.shared.gate.is_open());
    }
}

#[test]
fn native_policy_wait_resamples_before_checking_the_original_item_deadline() {
    for elapsed in [999, 1_000] {
        let mut boundary = HostBoundary::new(2_000);
        let until = HostInstant::from_micros(1_000);
        let allowed = boundary.during_policy_read(Some(until), |boundary| {
            boundary.clock.advance_to(Time::from_nanos(elapsed * 1_000));
        });
        assert_eq!(allowed, elapsed < 1_000);
        assert!(boundary.check(None));
        assert_eq!(boundary.check(Some(until)), elapsed < 1_000);
        assert!(boundary.seed.shared.gate.is_open());
    }
}

#[test]
fn newer_policy_cannot_replace_a_cap_retained_before_the_native_wait() {
    let mut boundary = HostBoundary::new(1_000);
    assert!(!boundary.during_policy_read(None, |boundary| {
        boundary.clock.advance_to(Time::from_millis(1));
        boundary.policy.until.store(2_000, Ordering::Release);
    }));
    assert!(boundary.check(None));
    assert!(!boundary.check(Some(HostInstant::from_micros(1_000))));
    assert!(!boundary.policy.revoked.load(Ordering::Acquire));
}

#[test]
fn native_revocation_during_the_policy_wait_is_seen_before_returning_permission() {
    let mut boundary = HostBoundary::new(2_000);
    assert!(!boundary.during_policy_read(Some(HostInstant::from_micros(1_000)), |boundary| {
        boundary.clock.advance_to(Time::from_nanos(999_000));
        boundary.policy.revoked.store(true, Ordering::Release);
    }));
    assert!(!boundary.check(None));
}

#[test]
fn native_cancellation_during_the_policy_wait_is_seen_before_returning_permission() {
    let mut boundary = HostBoundary::new(2_000);
    assert!(!boundary.during_policy_read(Some(HostInstant::from_micros(1_000)), |boundary| {
        boundary.clock.advance_to(Time::from_nanos(999_000));
        boundary
            .seed
            .cx
            .cancel_with(asupersync::types::CancelKind::User, None);
    }));
    assert!(!boundary.check(None));
}

#[test]
fn the_controller_native_guard_uses_its_own_cursor_and_keeps_the_original_item_cap() {
    use crate::test_support::authority;
    let (clock, _runtime, cx) = native_runtime();
    let grant = authority::grant(&cx);
    let mut input = authority::viewer(&cx, grant);
    let lane = input.attach_clipboard(77, true, ClientInstant(0)).unwrap();
    lane.local_switch().set_enabled(true);
    lane.peer_switch().set_enabled(true);
    // The separate network consumer may overtake this worker without changing
    // its clock cursor or letting it reauthorize the native channel's sample.
    let network = lane.transport();
    network.sample(ClientInstant(10)).unwrap();
    let worker = lane.transport();
    let gate = worker.transport();
    let seed = native_seed(
        Session::Controller(lane),
        gate,
        Clock::Controller(worker),
        cx.clone(),
    );
    let access = seed.native_access();
    let until = HostInstant::from_micros(1_000);
    assert!(access.check(Some(until)));
    clock.advance_to(Time::from_nanos(999_000));
    assert!(access.check(Some(until)));
    clock.advance_to(Time::from_millis(1));
    assert!(!access.check(Some(until)));
    assert!(access.check(None));

    input.suspend(ClientInstant(1_000));
    assert!(!access.check(None));
    clock.advance_to(Time::from_millis(2));
    authority::present(&cx, &mut input, grant, 2);
    authority::renew(&cx, &mut input, grant);
    assert_eq!(input.suspended_since(), None);
    assert!(access.check(None));
    assert!(!access.check(Some(until)));

    drop(input);
    assert!(seed.shared.gate.is_open());
    assert!(!access.check(None));
}
