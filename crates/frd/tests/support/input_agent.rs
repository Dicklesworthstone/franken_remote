//! Common original input authority, bounded wire records and native sink fixtures.
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_submission::*,
    limits::ProtocolLimits,
    time::HostDuration,
};
use fr_wire::input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, encode_input};
use frd::{
    input_agent::{Agent, Driver, Reply, Route, Shutdown},
    input_watchdog::host_now,
};
use std::{
    num::NonZeroU64,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

pub(super) fn runtime() -> Runtime {
    RuntimeBuilder::new().worker_threads(1).build().unwrap()
}
pub(super) fn runtime_cx() -> (Runtime, Cx) {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    (rt, cx)
}
pub(super) fn credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}
pub(super) fn session(cx: &Cx, lease_us: u64) -> InputSession {
    session_with_presented(cx, lease_us, None)
}
pub(super) fn session_with_presented(
    cx: &Cx,
    lease_us: u64,
    applied: Option<NonZeroU64>,
) -> InputSession {
    let now = host_now(cx).unwrap();
    let c = credentials();
    let mut a = SessionAuthority::new(
        c.session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(lease_us),
            ticket_lifetime: HostDuration::from_micros(lease_us / 2),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(now).unwrap();
    if let Some(sequence) = applied {
        a.mark_presented_ready_until(
            sequence,
            now.checked_add(HostDuration::from_micros(lease_us))
                .unwrap(),
            now,
        )
        .unwrap();
    } else {
        a.mark_view_ready(now).unwrap();
    }
    a.grant_lease(c.lease, now).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, now).unwrap();
    InputSession::new(
        a,
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default()
            .with(Capability::Keys)
            .with(Capability::Buttons)
            .with(Capability::Absolute)
            .with(Capability::Text),
        now,
    )
    .unwrap()
}
pub(super) fn key(pressed: bool) -> InputEvent<'static> {
    InputEvent::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: if pressed {
            KeyTransition::Press
        } else {
            KeyTransition::Release
        },
    }
}
pub(super) fn bytes(seq: u64, event: InputEvent<'_>) -> Vec<u8> {
    let mut b = vec![0; MAX_INPUT_RECORD_BYTES];
    let n = encode_input(
        InputRequest {
            credentials: credentials(),
            sequence: seq,
            event,
        },
        &mut b,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    b.truncate(n);
    b
}
pub(super) fn eventually(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < until, "native condition timed out");
        thread::sleep(Duration::from_millis(1));
    }
}
pub(super) fn reply(agent: &mut Agent) -> Reply {
    let mut value = None;
    eventually(|| {
        value = agent.try_reply().unwrap();
        value.is_some()
    });
    value.unwrap()
}
pub(super) fn submitted(value: Reply) -> Receipt {
    let Reply::Input(Ok(Dispatch::Completed(r))) = value else {
        panic!("expected submission receipt: {value:?}")
    };
    r
}
pub(super) struct Runner {
    pub(super) done: mpsc::Receiver<Shutdown>,
    pub(super) join: thread::JoinHandle<()>,
}
impl Runner {
    pub(super) fn start(runtime: Runtime, driver: Driver) -> Self {
        let (tx, done) = mpsc::sync_channel(1);
        let join = thread::spawn(move || {
            let _ = tx.send(runtime.block_on(driver));
        });
        Self { done, join }
    }
    pub(super) fn finish(self) -> Shutdown {
        let r = self.done.recv_timeout(Duration::from_secs(4)).unwrap();
        self.join.join().unwrap();
        r
    }
}
#[derive(Default)]
pub(super) struct Trace {
    pub(super) held: AtomicBool,
    pub(super) effects: Mutex<Vec<Operation>>,
    pub(super) clean: AtomicBool,
    pub(super) drops: AtomicUsize,
}
pub(super) struct Sink {
    pub(super) trace: Arc<Trace>,
    pub(super) block_prepare: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
    pub(super) block_submit: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
    pub(super) drop_block: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
    pub(super) panic_on_second: bool,
}
impl Sink {
    pub(super) fn new(t: Arc<Trace>) -> Self {
        Self {
            trace: t,
            block_prepare: None,
            block_submit: None,
            drop_block: None,
            panic_on_second: false,
        }
    }
}
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        if let Some((entered, resume)) = self.block_prepare.take() {
            entered.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(4)).unwrap();
        }
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        if let Some((entered, resume)) = self.block_submit.take() {
            entered.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(4)).unwrap();
        }
        let mut e = self.trace.effects.lock().unwrap();
        assert!(
            !(self.panic_on_second && e.len() == 1),
            "injected native failure"
        );
        if let Operation::Key { transition, .. } = op {
            self.trace
                .held
                .store(transition != KeyTransition::Release, Ordering::Release);
        }
        e.push(op);
        Submission::Submitted
    }
}
impl Drop for Sink {
    fn drop(&mut self) {
        if let Some((entered, resume)) = self.drop_block.take() {
            entered.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(4)).unwrap();
        }
        self.trace.drops.fetch_add(1, Ordering::Release);
    }
}
pub(super) fn clean(s: &mut Sink) -> bool {
    s.trace.clean.load(Ordering::Acquire)
}
pub(super) fn trace() -> Arc<Trace> {
    Arc::new(Trace {
        clean: AtomicBool::new(true),
        ..Trace::default()
    })
}
pub(super) fn route() -> Route {
    Route::new(7, ProtocolLimits::ABSOLUTE)
}
