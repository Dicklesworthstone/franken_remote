//! Suspension is not a new grant, a stopped item clock, or publication authority.
use fr_core::{
    authority::{AuthorityError, AuthorityPolicy, SessionAuthority},
    clipboard::{
        Begin, Binding, ClipboardSession, ClipboardSink, Endpoint, Error, PlatformError,
        Publication, Stamp,
        authority::{Authority, Monitor},
    },
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{Capabilities, ControlLease, InputSession, Refusal},
    limits::ProtocolLimits,
    time::HostInstant,
};
use std::sync::{
    Arc, Mutex,
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

struct RealHost {
    authority: Arc<Mutex<SessionAuthority>>,
    input: InputSession,
    renewal: ControlLease,
}
impl RealHost {
    fn new() -> Self {
        let credentials = InputCredentials {
            session: binding().session,
            lease: binding().lease,
            ticket: InputTicketId::from_raw(3),
            view: InputView {
                geometry: DisplayGeometryGeneration::INITIAL,
                viewport: ViewportMappingGeneration::INITIAL,
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
        };
        let mut authority =
            SessionAuthority::new(credentials.session, AuthorityPolicy::plan_defaults());
        authority.mark_capabilities_checked().unwrap();
        authority.authorize_observation(at(0)).unwrap();
        authority.mark_view_ready_until(at(250_000), at(0)).unwrap();
        authority.grant_lease(credentials.lease, at(0)).unwrap();
        authority
            .issue_input_ticket(credentials.lease, credentials.ticket, at(0))
            .unwrap();
        let authority = Arc::new(Mutex::new(authority));
        let mut input = InputSession::from_shared_authority(
            authority.clone(),
            credentials,
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 16, 16).unwrap(),
            Capabilities::default(),
            at(0),
        )
        .unwrap();
        let renewal = input.take_control_lease().unwrap();
        Self {
            authority,
            input,
            renewal,
        }
    }
    fn renew_while_suspended(&mut self) {
        // The original bounded view lapses at 250 ms. Actual observation and
        // control responses keep this same lease alive beyond its 10 s limit;
        // neither renewal provides presentation evidence or revives a ticket.
        assert_eq!(self.renewal.deadline(at(250_000)), Ok(at(3_000_000)));
        for second in 1..=10_u64 {
            let now = at(second * 1_000_000);
            let nonce = u128::from(second);
            {
                let mut authority = self.authority.lock().unwrap();
                authority.issue_observation_challenge(nonce, now).unwrap();
                authority.respond_observation_challenge(nonce, now).unwrap();
            }
            self.renewal.challenge(nonce, now).unwrap();
            self.renewal.respond(nonce, now).unwrap();
        }
        assert!(!self.input.monitor().is_revoked());
    }
}

#[test]
fn real_host_deadline_queries_report_the_first_suspension_limit_read_as_terminal() {
    for item_deadline in [false, true] {
        let mut host = RealHost::new();
        let monitor = Monitor::from_input(&host.input);
        host.renew_while_suspended();
        assert_eq!(
            monitor.deadline(at(10_249_999)),
            Err(Refusal::Authority(AuthorityError::ViewUnready))
        );
        assert!(!host.input.monitor().is_revoked());
        let result = if item_deadline {
            monitor.item_deadline(at(10_250_000))
        } else {
            monitor.deadline(at(10_250_000))
        };
        assert_eq!(result, Err(Refusal::Revoked));
        assert!(host.input.monitor().is_revoked());
    }
}

#[test]
fn real_host_suspension_limit_closes_an_existing_clipboard_on_its_first_check() {
    let mut host = RealHost::new();
    let mut session = ClipboardSession::with_monitor(
        Monitor::from_input(&host.input),
        Endpoint::Host,
        ProtocolLimits::ABSOLUTE,
        true,
        at(0),
    )
    .unwrap();
    session.begin(begin(), at(0)).unwrap();
    host.renew_while_suspended();
    assert_eq!(session.maintain(at(10_249_999)), Err(Error::Suspended));
    assert!(!session.is_closed());
    assert_eq!(
        session.maintain(at(10_250_000)),
        Err(Error::Authority(Refusal::Revoked))
    );
    assert!(session.is_closed());
    assert_eq!(session.reserved_bytes(), 0);
}

#[test]
fn a_clipboard_constructor_at_the_real_suspension_limit_is_refused_immediately() {
    let mut host = RealHost::new();
    host.renew_while_suspended();
    let result = ClipboardSession::with_monitor(
        Monitor::from_input(&host.input),
        Endpoint::Host,
        ProtocolLimits::ABSOLUTE,
        true,
        at(10_250_000),
    );
    assert!(matches!(result, Err(Error::Authority(Refusal::Revoked))));
    assert!(host.input.monitor().is_revoked());
}
