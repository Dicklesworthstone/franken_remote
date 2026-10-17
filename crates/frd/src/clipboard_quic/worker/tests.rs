//! Deterministic native-worker lifecycle tests with an explicitly recording OS.
//! The production wire owner and bounded handoff slots are used; no live X11 or
//! network qualification is claimed by these tests.
use super::*;
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    time::{TimerDriverHandle, VirtualClock},
    types::{Budget, Time},
};
use fr_core::{
    authority::AuthorityError,
    clipboard::{
        Binding, ClipboardSink, Publication, Stamp,
        authority::{Authority, Monitor},
    },
    ids::{InputLeaseId, RemoteSessionId},
    input_submission::Refusal,
    limits::ProtocolLimits,
};
use fr_wire::clipboard::{
    Body, Context, Lane, Role, decode,
    session::synchronize::{NativeChange, NativeChanges, NativeText},
};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
struct Permission {
    paused: AtomicBool,
    revoked: AtomicBool,
}
struct Owner(Arc<Permission>);
impl Authority for Owner {
    fn binding(&self) -> Binding {
        binding()
    }
    fn deadline(&self, now: HostInstant) -> Result<HostInstant, Refusal> {
        if self.0.revoked.load(Ordering::Acquire) {
            return Err(Refusal::Revoked);
        }
        if now.as_micros() >= 10_000_000 {
            return Err(Refusal::Authority(AuthorityError::LeaseExpired));
        }
        if self.0.paused.load(Ordering::Acquire) {
            return Err(Refusal::Authority(AuthorityError::ViewUnready));
        }
        Ok(HostInstant::from_micros(10_000_000))
    }
    fn revoke(&self) {
        self.0.revoked.store(true, Ordering::Release);
    }
}
fn binding() -> Binding {
    Binding {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
    }
}
fn context() -> Context {
    Context {
        scope: binding(),
        channel: 3,
        sender: Role::Host,
        lane: Lane::Clipboard,
    }
}

#[derive(Default)]
struct Os {
    text: Option<String>,
    revision: u64,
    opens: usize,
    watches: usize,
    changes: usize,
    reads: usize,
    publications: usize,
    suspends: usize,
    closes: usize,
}
struct Recording {
    os: Arc<Mutex<Os>>,
    seen: u64,
    reading: bool,
}
struct Text(String);
impl NativeText for Text {
    fn text(&self) -> &str {
        &self.0
    }
    fn origin(&self) -> Option<Stamp> {
        None
    }
}
impl ClipboardSink for Recording {
    fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
        Ok(())
    }
    fn publish(&mut self, _: &str, _: Stamp) -> Publication {
        self.os.lock().unwrap().publications += 1;
        Publication::SubmittedToOs
    }
}
impl NativeClipboard for Recording {
    type Text = Text;
    type Error = ();
    fn watch(&mut self) -> Result<u64, ()> {
        self.os.lock().unwrap().watches += 1;
        Ok(self.seen)
    }
    fn changes(&mut self) -> Result<NativeChanges, ()> {
        let mut os = self.os.lock().unwrap();
        os.changes += 1;
        let latest = (os.revision != self.seen).then_some(NativeChange {
            revision: os.revision,
            has_selection: os.text.is_some(),
            origin: None,
        });
        self.seen = os.revision;
        Ok(NativeChanges {
            latest,
            settled: true,
        })
    }
    fn revision(&self) -> u64 {
        self.os.lock().unwrap().revision
    }
    fn prepare_for_revision(
        &mut self,
        _: &str,
        _: Stamp,
        revision: u64,
    ) -> Result<(), PlatformError> {
        if revision == self.revision() {
            Ok(())
        } else {
            Err(PlatformError::LocalChanged)
        }
    }
    fn begin_read(&mut self) -> Result<(), ()> {
        self.os.lock().unwrap().reads += 1;
        self.reading = true;
        Ok(())
    }
    fn poll_read(&mut self) -> Result<Option<Text>, ()> {
        if !self.reading {
            return Ok(None);
        }
        self.reading = false;
        Ok(self.os.lock().unwrap().text.clone().map(Text))
    }
    fn cancel_read(&mut self) {
        self.reading = false;
    }
    fn suspend(&mut self) -> Result<(), ()> {
        let mut os = self.os.lock().unwrap();
        os.suspends += 1;
        // Match the important native consequence: disabling erases the served
        // remote selection. A transient view pause must not invoke this.
        os.text = None;
        Ok(())
    }
    fn close(&mut self) {
        self.os.lock().unwrap().closes += 1;
    }
}

struct Fixture {
    clock: Arc<VirtualClock>,
    permission: Arc<Permission>,
    os: Arc<Mutex<Os>>,
    shared: Arc<Shared>,
    worker: Worker<Recording>,
    _runtime: Runtime,
}
impl Fixture {
    fn new(paused: bool) -> Self {
        let clock = Arc::new(VirtualClock::new());
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .with_timer_driver(TimerDriverHandle::with_virtual_clock(clock.clone()))
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let permission = Arc::new(Permission::default());
        let channel = ChannelSession::with_monitor(
            Monitor::new(Owner(permission.clone())),
            context(),
            ProtocolLimits::ABSOLUTE,
            true,
            HostInstant::ORIGIN,
        )
        .unwrap();
        let gate = channel.transport();
        let shared = Arc::new(Shared {
            gate: gate.clone(),
            maximum: ProtocolLimits::ABSOLUTE.max_control_message_bytes() as usize,
            inbox: Mutex::default(),
            outbox: Mutex::default(),
            inbound_until: std::sync::atomic::AtomicU64::default(),
            wake: std::sync::OnceLock::default(),
        });
        let seed = WorkerSeed {
            session: Some(Session::Host(channel)),
            shared: shared.clone(),
            clock: Clock::Host(gate),
            cx,
        };
        let os = Arc::new(Mutex::new(Os::default()));
        permission.paused.store(paused, Ordering::Release);
        let native = os.clone();
        let worker = seed
            .open(move || {
                native.lock().unwrap().opens += 1;
                Ok(Recording {
                    os: native,
                    seen: 0,
                    reading: false,
                })
            })
            .unwrap();
        Self {
            clock,
            permission,
            os,
            shared,
            worker,
            _runtime: runtime,
        }
    }
    fn copy(&self, text: &str) {
        let mut os = self.os.lock().unwrap();
        os.revision += 1;
        os.text = Some(text.into());
    }
    fn step(&mut self) -> Result<(), Error> {
        self.worker.step(|| Ok(91))
    }
}

#[test]
fn opening_while_paused_keeps_one_worker_without_reading_or_watching() {
    let mut f = Fixture::new(true);
    f.copy("selection made during pause");
    f.step().unwrap();
    let os = f.os.lock().unwrap();
    assert_eq!(
        (
            os.opens,
            os.watches,
            os.changes,
            os.reads,
            os.publications,
            os.suspends
        ),
        (1, 0, 0, 0, 0, 0)
    );
    assert!(f.shared.gate.is_open());
    assert!(f.shared.outbox.lock().unwrap().item.is_none());
}

#[test]
fn pause_keeps_subscription_and_sends_the_latest_local_copy_after_resume() {
    let mut f = Fixture::new(false);
    f.step().unwrap();
    let changes = f.os.lock().unwrap().changes;
    f.permission.paused.store(true, Ordering::Release);
    f.copy("superseded while paused");
    f.step().unwrap();
    f.copy("latest while paused");
    f.clock.advance_to(Time::from_millis(500));
    f.step().unwrap();
    assert_eq!(f.os.lock().unwrap().changes, changes);
    assert!(f.shared.outbox.lock().unwrap().item.is_none());
    f.permission.paused.store(false, Ordering::Release);
    let mut chunks = Vec::new();
    for _ in 0..3 {
        f.step().unwrap();
        let mut outbox = f.shared.outbox.lock().unwrap();
        let record = outbox
            .item
            .take()
            .expect("begin, chunk and commit after resume");
        outbox.busy = false;
        let message = decode(&record.bytes.0, context(), &ProtocolLimits::ABSOLUTE).unwrap();
        if let Body::Chunk { bytes, .. } = message.body {
            chunks.extend_from_slice(bytes);
        }
    }
    assert_eq!(chunks, b"latest while paused");
    let os = f.os.lock().unwrap();
    assert_eq!(
        (os.opens, os.watches, os.reads, os.suspends, os.closes),
        (1, 1, 1, 0, 0)
    );
    assert!(f.shared.gate.is_open());
}

#[test]
fn queued_ingress_keeps_its_original_deadline_during_pause() {
    let mut f = Fixture::new(false);
    f.permission.paused.store(true, Ordering::Release);
    // The dispatcher owns framing; this deliberately unread body must never
    // reach a parser/native owner before its fixed handoff lifetime expires.
    assert_eq!(
        f.shared
            .accept(b"unconsumed record", 0, HostInstant::ORIGIN),
        Ok(super::super::Disposition::Consumed)
    );
    f.step().unwrap();
    assert!(f.shared.gate.is_open());
    f.clock.advance_to(Time::from_millis(3_000));
    assert_eq!(f.step(), Err(Error::HandoffExpired));
    assert!(!f.shared.gate.is_open());
    assert_eq!(f.os.lock().unwrap().publications, 0);
    f.permission.paused.store(false, Ordering::Release);
    assert_eq!(f.step(), Err(Error::Closed));
}

#[test]
fn revocation_remains_terminal_while_the_worker_is_paused() {
    let mut f = Fixture::new(true);
    f.step().unwrap();
    f.permission.revoked.store(true, Ordering::Release);
    assert!(f.step().is_err());
    assert!(!f.shared.gate.is_open());
    f.permission.revoked.store(false, Ordering::Release);
    f.permission.paused.store(false, Ordering::Release);
    assert_eq!(f.step(), Err(Error::Closed));
    let os = f.os.lock().unwrap();
    assert_eq!((os.watches, os.reads, os.publications), (0, 0, 0));
}
