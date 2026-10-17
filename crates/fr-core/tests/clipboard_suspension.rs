//! Suspension is not a new grant, a stopped item clock, or publication authority.
use fr_core::{
    authority::AuthorityError,
    clipboard::{
        Begin, Binding, ClipboardSession, ClipboardSink, Endpoint, Error, PlatformError,
        Publication, Stamp,
        authority::{Authority, Monitor},
    },
    ids::{InputLeaseId, RemoteSessionId},
    input_submission::Refusal,
    limits::ProtocolLimits,
    time::HostInstant,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

struct State {
    paused: AtomicBool,
    revoked: AtomicBool,
    until: AtomicU64,
}
struct Owner(Arc<State>);
impl Authority for Owner {
    fn binding(&self) -> Binding {
        binding()
    }
    fn deadline(&self, now: HostInstant) -> Result<HostInstant, Refusal> {
        if self.0.revoked.load(Ordering::Acquire) {
            return Err(Refusal::Revoked);
        }
        let until = at(self.0.until.load(Ordering::Acquire));
        if now >= until {
            return Err(Refusal::Authority(AuthorityError::LeaseExpired));
        }
        if self.0.paused.load(Ordering::Acquire) {
            return Err(Refusal::Authority(AuthorityError::ViewUnready));
        }
        Ok(until)
    }
    fn revoke(&self) {
        self.0.revoked.store(true, Ordering::Release);
    }
}
fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
fn binding() -> Binding {
    Binding {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
    }
}
fn begin() -> Begin {
    Begin {
        binding: binding(),
        stamp: Stamp {
            id: 3,
            source: Endpoint::Controller,
            sequence: 1,
        },
        total_bytes: 2,
        chunks: 2,
    }
}
fn setup() -> (Arc<State>, ClipboardSession) {
    let state = Arc::new(State {
        paused: AtomicBool::new(false),
        revoked: AtomicBool::new(false),
        until: AtomicU64::new(10_000_000),
    });
    let session = ClipboardSession::with_monitor(
        Monitor::new(Owner(state.clone())),
        Endpoint::Host,
        ProtocolLimits::ABSOLUTE,
        true,
        at(0),
    )
    .unwrap();
    (state, session)
}
#[derive(Default)]
struct Sink {
    prepare_pause: Option<Arc<State>>,
    prepared: usize,
    published: usize,
    cancelled: usize,
}
impl ClipboardSink for Sink {
    fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
        self.prepared += 1;
        if let Some(state) = &self.prepare_pause {
            state.paused.store(true, Ordering::Release);
        }
        Ok(())
    }
    fn publish(&mut self, _: &str, _: Stamp) -> Publication {
        self.published += 1;
        Publication::SubmittedToOs
    }
    fn cancel_prepared(&mut self) {
        self.cancelled += 1;
    }
}

#[test]
fn suspended_receive_resumes_original_cursor_without_admitting_work_while_stale() {
    let (state, mut session) = setup();
    let b = begin();
    session.begin(b, at(0)).unwrap();
    session.chunk(b.stamp, 0, 0, b"a", at(1)).unwrap();
    let reserved = session.reserved_bytes();
    state.paused.store(true, Ordering::Release);
    assert_eq!(session.maintain(at(2)), Err(Error::Suspended));
    assert_eq!(
        session.chunk(b.stamp, 1, 1, b"b", at(3)),
        Err(Error::Suspended)
    );
    let mut sink = Sink::default();
    assert_eq!(
        session.commit(b.stamp, 2, &mut sink, || at(4)),
        Err(Error::Suspended)
    );
    assert_eq!(session.local_change(None, at(5)), Err(Error::Suspended));
    assert_eq!(session.buffered_bytes(), 1);
    assert_eq!(session.reserved_bytes(), reserved);
    assert_eq!((sink.prepared, sink.published), (0, 0));
    assert!(!session.is_closed());
    state.paused.store(false, Ordering::Release);
    session.chunk(b.stamp, 1, 1, b"b", at(6)).unwrap();
    let receipt = session.commit(b.stamp, 2, &mut sink, || at(7)).unwrap();
    assert_eq!(receipt.publication, Publication::SubmittedToOs);
    assert_eq!(sink.published, 1);
    assert_eq!(session.begin(b, at(8)), Err(Error::Replay));
}

#[test]
fn silence_while_suspended_expires_original_item_and_never_resets_sequence_floor() {
    let (state, mut session) = setup();
    let b = begin();
    session.begin(b, at(0)).unwrap();
    state.paused.store(true, Ordering::Release);
    assert_eq!(session.maintain(at(2_999_999)), Err(Error::Suspended));
    assert!(session.reserved_bytes() > 0);
    assert_eq!(session.maintain(at(3_000_000)), Err(Error::Suspended));
    assert_eq!(session.reserved_bytes(), 0);
    state.paused.store(false, Ordering::Release);
    assert_eq!(
        session.chunk(b.stamp, 0, 0, b"a", at(3_000_001)),
        Err(Error::UnknownTransfer)
    );
    assert_eq!(session.begin(b, at(3_000_002)), Err(Error::Replay));
    assert!(!session.is_closed());
}

#[test]
fn renewed_authority_does_not_extend_an_already_admitted_item() {
    let (state, mut session) = setup();
    state.until.store(100, Ordering::Release);
    let b = begin();
    session.begin(b, at(0)).unwrap();
    state.paused.store(true, Ordering::Release);
    session.maintain(at(50)).unwrap_err();
    state.until.store(1_000, Ordering::Release);
    state.paused.store(false, Ordering::Release);
    assert_eq!(session.maintain(at(100)), Err(Error::Expired));
    assert_eq!(session.reserved_bytes(), 0);
    assert!(!session.is_closed());
    assert_eq!(session.begin(b, at(101)), Err(Error::Replay));
}

#[test]
fn off_on_while_suspended_cancels_the_old_transfer() {
    let (state, mut session) = setup();
    let b = begin();
    session.begin(b, at(0)).unwrap();
    state.paused.store(true, Ordering::Release);
    let switch = session.local_switch();
    switch.set_enabled(false);
    switch.set_enabled(true);
    assert_eq!(session.maintain(at(1)), Err(Error::Disabled));
    assert_eq!(session.reserved_bytes(), 0);
    state.paused.store(false, Ordering::Release);
    assert_eq!(session.begin(b, at(2)), Err(Error::Replay));
}

#[test]
fn suspension_during_preparation_cancels_without_publication_or_retry() {
    let (state, mut session) = setup();
    let b = begin();
    session.begin(b, at(0)).unwrap();
    session.chunk(b.stamp, 0, 0, b"a", at(1)).unwrap();
    session.chunk(b.stamp, 1, 1, b"b", at(2)).unwrap();
    let mut sink = Sink {
        prepare_pause: Some(state.clone()),
        ..Sink::default()
    };
    assert_eq!(
        session.commit(b.stamp, 2, &mut sink, || at(3)),
        Err(Error::Suspended)
    );
    assert_eq!((sink.prepared, sink.published, sink.cancelled), (1, 0, 1));
    assert!(!session.is_closed());
    state.paused.store(false, Ordering::Release);
    assert_eq!(
        session.commit(b.stamp, 2, &mut sink, || at(4)),
        Err(Error::UnknownTransfer)
    );
    assert_eq!(session.begin(b, at(5)), Err(Error::Replay));
}

#[test]
fn revoked_or_expired_owner_is_terminal_even_during_suspension() {
    for revoked in [false, true] {
        let (state, mut session) = setup();
        session.begin(begin(), at(0)).unwrap();
        state.paused.store(true, Ordering::Release);
        if revoked {
            state.revoked.store(true, Ordering::Release);
        } else {
            state.until.store(1, Ordering::Release);
        }
        assert!(matches!(session.maintain(at(1)), Err(Error::Authority(_))));
        assert!(session.is_closed());
        assert_eq!(session.reserved_bytes(), 0);
        state.paused.store(false, Ordering::Release);
        state.revoked.store(false, Ordering::Release);
        state.until.store(10_000_000, Ordering::Release);
        assert_eq!(session.maintain(at(2)), Err(Error::Closed));
    }
}

#[test]
fn clock_regression_while_suspended_revokes_original_authority() {
    let (state, mut session) = setup();
    state.paused.store(true, Ordering::Release);
    assert_eq!(session.maintain(at(10)), Err(Error::Suspended));
    assert_eq!(session.maintain(at(9)), Err(Error::Clock));
    assert!(session.is_closed());
    assert!(state.revoked.load(Ordering::Acquire));
}

#[test]
fn attachment_during_suspension_retains_one_owner_without_admitting_payloads() {
    let (state, original) = setup();
    drop(original);
    state.paused.store(true, Ordering::Release);
    let mut session = ClipboardSession::with_monitor(
        Monitor::new(Owner(state.clone())),
        Endpoint::Host,
        ProtocolLimits::ABSOLUTE,
        true,
        at(1),
    )
    .unwrap();
    assert_eq!(session.begin(begin(), at(1)), Err(Error::Suspended));
    assert_eq!(session.reserved_bytes(), 0);
    assert!(!session.is_closed());
    state.paused.store(false, Ordering::Release);
    session.begin(begin(), at(2)).unwrap();
    assert!(session.reserved_bytes() > 0);
}
