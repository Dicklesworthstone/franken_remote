//! The full broker request -> real native startup -> grant -> input path.
//! Parent admission, installed input routes and visibility are explicit fixtures.
use super::*;
use fr_core::{
    input_submission::{InputSink, Operation, PlatformError, Submission},
    time::HostDuration,
};
use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample};
use fr_wire::{
    control::{GRANTED_BYTES, Granted, Request, Target},
    negotiation::{Capability, ControlBinding, Offer, Role as SessionRole, Selection},
};
use frd::input_quic::grant::{CAPABILITY, Error as GrantError, Event, GrantBroker, Scope};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};

fn selection() -> Selection {
    Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: SessionRole::RequestControl,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: vec![Capability {
            name: CAPABILITY.into(),
            version: 1,
            required: true,
        }],
    }
    .select()
    .unwrap()
}
struct Setup {
    cx: Cx,
    pair: Pair,
    observation: frd::media::ObservationControl,
    broker: GrantBroker,
    seat: Seat,
    request: Request,
    requester: fr_client::control_grant::RequestControl,
    clock: ClockCorrelation,
    observer: X11Pointer,
    display: Display,
}
impl Setup {
    async fn new(cx: Cx, seat: Seat, policy: AuthorityPolicy, ready: bool) -> Self {
        let pair = pair_with_feedback(&cx, true).await;
        Self::from_pair(cx, seat, policy, ready, pair, None)
    }
    async fn negotiated(
        cx: Cx,
        seat: Seat,
        policy: AuthorityPolicy,
        ready: bool,
    ) -> (Self, frd::input_quic::NegotiatedInput) {
        let (pair, host, viewer, selected) = super::negotiated::joined_selection(&cx).await;
        let setup = Self::from_pair(cx, seat, policy, ready, pair, Some((host, selected)));
        viewer
            .check_request(&setup.pair.client, setup.request)
            .unwrap();
        (setup, viewer)
    }
    fn from_pair(
        cx: Cx,
        seat: Seat,
        policy: AuthorityPolicy,
        ready: bool,
        pair: Pair,
        attached: Option<(frd::input_quic::NegotiatedInput, Selection)>,
    ) -> Self {
        let display = Display::new();
        let observer = X11Pointer::open(&display.name).unwrap();
        let request = Request {
            parent: if attached.is_some() {
                super::negotiated::parent()
            } else {
                ControlBinding {
                    id: pair.control_routes.inbound.binding,
                    host_boot: HostBootId::from_raw(4),
                    os_session: OsSessionId::from_raw(5),
                    remote_session: credentials().session,
                }
            },
            sequence: 0,
            target: Target {
                display_binding: if attached.is_some() { 6 } else { 12 },
                view: credentials().view,
                bounds: observer.bounds(),
                capabilities: observer.capabilities(),
            },
        };
        let now = host_now(&cx).unwrap();
        let mut a = SessionAuthority::new(request.parent.remote_session, policy);
        a.mark_capabilities_checked().unwrap();
        a.authorize_observation(now).unwrap();
        if ready {
            a.mark_view_ready(now).unwrap();
        }
        let observation = frd::media::ObservationControl::new(cx.clone(), a).unwrap();
        let selected = attached.as_ref().map_or_else(selection, |(_, s)| s.clone());
        let scope = Scope {
            parent: request.parent,
            control: pair.control_routes,
            selection: &selected,
        };
        let broker = if let Some((proof, _)) = attached {
            GrantBroker::from_negotiated(
                observation.clone(),
                &pair.server,
                seat.clone(),
                scope,
                proof,
            )
        } else {
            GrantBroker::new(
                observation.clone(),
                &pair.server,
                seat.clone(),
                scope,
                pair.routes,
            )
        }
        .unwrap();
        let requester = fr_client::control_grant::RequestControl::new(
            request,
            7,
            ProtocolLimits::ABSOLUTE,
            ClientInstant(now.as_micros()),
        )
        .unwrap();
        let clock = ClockCorrelation::new(
            ClockSample {
                host_boot: request.parent.host_boot,
                client_sent_us: now.as_micros(),
                client_received_us: now.as_micros(),
                host_sample_us: now.as_micros(),
            },
            ClockPolicy {
                drift_ppm: 0,
                ..ClockPolicy::default()
            },
        )
        .unwrap();
        Self {
            cx,
            pair,
            observation,
            broker,
            seat,
            request,
            requester,
            clock,
            observer,
            display,
        }
    }
    async fn pump(&mut self) {
        let (c, s) = Box::pin(network::both(
            self.pair
                .client
                .drive(&self.cx, Duration::from_millis(1), || true),
            self.broker
                .drive(&mut self.pair.server, Duration::from_millis(1)),
        ))
        .await;
        c.unwrap();
        s.unwrap();
    }
    async fn request(&mut self) {
        let now = ClientInstant(network::clock(&self.cx));
        let deadline = self.requester.deadline().0;
        let bytes = self.requester.pending(now).unwrap().unwrap().to_vec();
        loop {
            assert!(
                network::clock(&self.cx) < deadline,
                "request admission expired"
            );
            match self.pair.client.send(
                &self.cx,
                Route::Stream(StreamRoute {
                    outbound: true,
                    ..self.pair.control_routes.inbound
                }),
                &bytes,
                deadline,
                || true,
            ) {
                Ok(()) => break,
                Err(quic::Error::Backpressure) => self.pump().await,
                Err(e) => panic!("request transport refusal: {e:?}"),
            }
        }
        self.requester
            .sent(ClientInstant(network::clock(&self.cx)))
            .unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        while self.broker.request().is_none() {
            assert!(Instant::now() < until);
            self.pump().await;
            self.broker
                .receive(&mut self.pair.server, |_, _| {
                    panic!("unexpected grant request route")
                })
                .unwrap();
        }
        assert_eq!(self.broker.request(), Some(self.request));
        assert!(!self.seat.is_occupied());
    }
    fn approve(&mut self) -> Driver {
        let target = self.request.target;
        let factory = fr_native::input_agent::x11_factory(
            &self.display.name,
            target.bounds,
            target.capabilities,
        )
        .unwrap();
        self.broker
            .approve(
                &self.pair.server,
                target,
                || Some((InputLeaseId::from_raw(20), InputTicketId::from_raw(30))),
                factory,
                X11Pointer::cleanup_native,
            )
            .unwrap()
    }
    async fn queued(&mut self) {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            assert!(Instant::now() < until);
            if self
                .broker
                .service(&mut self.pair.server, Some(self.request.target))
                .unwrap()
                == Event::GrantQueued
            {
                break;
            }
            self.pump().await;
        }
    }
    async fn finish(mut self) -> (Fixture, Granted) {
        self.queued().await;
        let input = self
            .broker
            .finish(&self.pair.server, Some(self.request.target))
            .unwrap();
        let mut received = None;
        let until = Instant::now() + Duration::from_secs(2);
        while received.is_none() {
            assert!(Instant::now() < until);
            let (c, s) = Box::pin(network::both(
                self.pair
                    .client
                    .drive(&self.cx, Duration::from_millis(1), || true),
                self.pair
                    .server
                    .drive(&self.cx, Duration::from_millis(1), || {
                        self.observation.check().is_ok()
                    }),
            ))
            .await;
            c.unwrap();
            s.unwrap();
            self.pair
                .client
                .receive(
                    &self.cx,
                    || true,
                    |route, bytes| {
                        assert_eq!(
                            route,
                            Route::Stream(StreamRoute {
                                outbound: false,
                                ..self.pair.control_routes.outbound
                            })
                        );
                        assert!(received.is_none());
                        let b: [u8; GRANTED_BYTES] = bytes.try_into().unwrap();
                        received = Some(b);
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        let now = ClientInstant(network::clock(&self.cx));
        let (grant, mut client) = self
            .requester
            .accept(
                &received.unwrap(),
                self.clock,
                ClientPolicy {
                    view_age_us: 1_500_000,
                    receipt_timeout_us: 2_000_000,
                },
                now,
            )
            .unwrap();
        let mut action = [0; 256];
        assert_eq!(
            client.action(shift(KeyTransition::Press), &mut action, now),
            Err(fr_client::input::Error::MappingUnconfirmed)
        );
        client
            .confirm_mapping(grant.credentials().session, grant.credentials().view, now)
            .unwrap();
        assert_eq!(
            client.action(shift(KeyTransition::Press), &mut action, now),
            Err(fr_client::input::Error::NoPresentedView)
        );
        client
            .presented(
                PresentedObservation {
                    session: grant.credentials().session,
                    serial: 1,
                    view: grant.credentials().view,
                    received_at: now,
                    source_age_upper_us: 0,
                },
                now,
            )
            .unwrap();
        (
            Fixture {
                cx: self.cx,
                pair: self.pair,
                input,
                client,
                observer: self.observer,
                observation: self.observation,
                seat: self.seat,
                driver: None,
                receipts: vec![],
                tickets: vec![],
                clock: self.clock,
                obsolete_pointers: 0,
                _display: self.display,
            },
            grant,
        )
    }
}
fn run<F, Fut>(f: F)
where
    F: FnOnce(Cx, Cx) -> Fut,
    Fut: Future<Output = ()>,
{
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let second = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        asupersync::time::timeout(cx.now(), Duration::from_secs(10), f(cx.clone(), second))
            .await
            .unwrap();
    });
}
#[test]
fn initial_broker_grant_reaches_real_shift_drag_and_preserves_ticket_sequence() {
    run(|cx, _second| async move {
        let mut s = Setup::new(cx, Seat::default(), AuthorityPolicy::plan_defaults(), true).await;
        s.request().await;
        assert_eq!(
            s.broker.service(&mut s.pair.server, None).unwrap(),
            Event::AwaitingApproval
        );
        let driver = s.approve();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let (mut f, g) = Box::pin(s.finish()).await;
            assert_eq!(g.first_action, 0);
            assert_eq!(g.first_pointer, 0);
            for (i, action) in [
                shift(KeyTransition::Press),
                button(true),
                button(false),
                shift(KeyTransition::Release),
            ]
            .into_iter()
            .enumerate()
            {
                let bytes = f.action(action);
                f.send(&bytes, Route::Stream(f.pair.actions));
                f.until_receipts(i + 1).await;
                assert_eq!(f.receipts[i].outcome, InputOutcome::SubmittedToOs);
                if i == 1 {
                    assert_eq!(f.observer.query_pointer().unwrap().1 & 0x0101, 0x0101);
                }
            }
            let until = Instant::now() + Duration::from_secs(2);
            while f.tickets.is_empty() {
                assert!(Instant::now() < until);
                f.input
                    .renew_ticket(
                        &mut f.pair.server,
                        || true,
                        || Some(InputTicketId::from_raw(31)),
                    )
                    .unwrap();
                f.turn().await;
            }
            assert_eq!(f.tickets[0].sequence, 1);
            assert_eq!(f.tickets[0].credentials.lease, g.lease);
            assert_eq!(f.client.pending_actions(), 0);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn request_is_not_consent_and_wrong_local_target_never_calls_native_factory() {
    run(|cx, _second| async move {
        let mut s = Setup::new(cx, Seat::default(), AuthorityPolicy::plan_defaults(), true).await;
        s.request().await;
        let mut wrong = s.request.target;
        wrong.display_binding += 1;
        let result = s.broker.approve::<X11Pointer, _, _>(
            &s.pair.server,
            wrong,
            || panic!("credential before approval"),
            || panic!("native before approval"),
            X11Pointer::cleanup_native,
        );
        assert!(matches!(result, Err(GrantError::TargetChanged)));
        assert!(!s.seat.is_occupied());
        assert!(s.observation.check().is_ok());
        s.broker.deny();
        assert!(s.broker.request().is_none());
        assert!(matches!(
            s.broker.approve::<X11Pointer, _, _>(
                &s.pair.server,
                s.request.target,
                || panic!(),
                || panic!(),
                X11Pointer::cleanup_native
            ),
            Err(GrantError::NoRequest)
        ));
        assert!(s.observation.check().is_ok());
    });
}
#[test]
fn approval_cannot_create_readiness_or_hold_the_seat_after_refusal() {
    run(|cx, _second| async move {
        let mut s = Setup::new(cx, Seat::default(), AuthorityPolicy::plan_defaults(), false).await;
        s.request().await;
        let result = s.broker.approve::<X11Pointer, _, _>(
            &s.pair.server,
            s.request.target,
            || Some((InputLeaseId::from_raw(20), InputTicketId::from_raw(30))),
            || panic!("factory must not run"),
            X11Pointer::cleanup_native,
        );
        assert!(matches!(result, Err(GrantError::Media(_))));
        assert!(!s.seat.is_occupied());
        assert!(s.observation.check().is_ok());
    });
}
#[test]
fn competing_approved_viewers_cannot_both_mint_authority_or_call_factories() {
    run(|cx, second| async move {
        let seat = Seat::default();
        let mut a = Setup::new(
            cx.clone(),
            seat.clone(),
            AuthorityPolicy::plan_defaults(),
            true,
        )
        .await;
        let mut b = Setup::new(second, seat.clone(), AuthorityPolicy::plan_defaults(), true).await;
        a.request().await;
        b.request().await;
        let driver = a.approve();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            assert!(seat.is_occupied());
            assert!(matches!(
                b.broker.approve::<X11Pointer, _, _>(
                    &b.pair.server,
                    b.request.target,
                    || panic!("busy must not consume credentials"),
                    || panic!("losing factory"),
                    X11Pointer::cleanup_native
                ),
                Err(GrantError::Agent(frd::input_agent::Error::SeatBusy))
            ));
            assert!(b.observation.check().is_ok());
            a.broker.stop();
        }))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
        assert!(a.observation.check().is_err());
        assert!(b.observation.check().is_ok());
        let driver = b.approve();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            b.queued().await;
            b.broker.stop();
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
struct EmptySink;
impl InputSink for EmptySink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        panic!("no grant, no preparation")
    }
    fn submit(&mut self, _: Operation) -> Submission {
        panic!("no grant, no native input")
    }
}
#[test]
fn blocked_initialization_expires_without_grant_and_keeps_seat_until_native_destruction() {
    run(|cx, _second| async move {
        let policy = AuthorityPolicy {
            authorization_lifetime: HostDuration::from_millis_checked(200).unwrap(),
            ticket_lifetime: HostDuration::from_millis_checked(60).unwrap(),
        };
        let mut s = Setup::new(cx, Seat::default(), policy, true).await;
        s.request().await;
        let (tx, rx) = mpsc::channel();
        let entered = Arc::new(AtomicBool::new(false));
        let worker = entered.clone();
        let driver = s
            .broker
            .approve(
                &s.pair.server,
                s.request.target,
                || Some((InputLeaseId::from_raw(20), InputTicketId::from_raw(30))),
                move || {
                    worker.store(true, Ordering::Release);
                    let _ = rx.recv();
                    Ok(EmptySink)
                },
                |_| true,
            )
            .unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            while !entered.load(Ordering::Acquire) {
                asupersync::time::sleep(s.cx.now(), Duration::from_millis(1)).await;
            }
            assert_eq!(
                s.broker
                    .service(&mut s.pair.server, Some(s.request.target))
                    .unwrap(),
                Event::NativeStarting
            );
            asupersync::time::sleep(s.cx.now(), Duration::from_millis(80)).await;
            assert_eq!(
                s.broker.service(&mut s.pair.server, Some(s.request.target)),
                Err(GrantError::Expired)
            );
            assert!(s.seat.is_occupied());
            assert!(s.broker.native_status().unwrap().stopped);
            assert!(s.broker.native_status().unwrap().exit.is_none());
            tx.send(()).unwrap();
        }))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!s.seat.is_occupied());
    });
}
#[test]
fn watchdog_revokes_blocked_initialization_without_broker_service() {
    run(|cx, _second| async move {
        let policy = AuthorityPolicy {
            authorization_lifetime: HostDuration::from_millis_checked(150).unwrap(),
            ticket_lifetime: HostDuration::from_millis_checked(100).unwrap(),
        };
        let mut s = Setup::new(cx, Seat::default(), policy, true).await;
        s.request().await;
        let (tx, rx) = mpsc::channel();
        let driver = s
            .broker
            .approve(
                &s.pair.server,
                s.request.target,
                || Some((InputLeaseId::from_raw(20), InputTicketId::from_raw(30))),
                move || {
                    let _ = rx.recv();
                    Ok(EmptySink)
                },
                |_| true,
            )
            .unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let end = Instant::now() + Duration::from_secs(1);
            while !s.broker.native_status().unwrap().stopped {
                assert!(Instant::now() < end);
                asupersync::time::sleep(s.cx.now(), Duration::from_millis(1)).await;
            }
            assert!(s.seat.is_occupied());
            assert!(s.broker.native_status().unwrap().exit.is_none());
            tx.send(()).unwrap();
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn native_initialization_failure_or_panic_cannot_publish_grant_or_claim_unsafe_handoff() {
    run(|cx, second| async move {
        for (panic, cx) in [false, true].into_iter().zip([cx, second]) {
            let mut s = Setup::new(
                cx.clone(),
                Seat::default(),
                AuthorityPolicy::plan_defaults(),
                true,
            )
            .await;
            s.request().await;
            let driver = s
                .broker
                .approve::<EmptySink, _, _>(
                    &s.pair.server,
                    s.request.target,
                    || Some((InputLeaseId::from_raw(20), InputTicketId::from_raw(30))),
                    move || {
                        assert!(!panic, "injected initialization panic");
                        Err(PlatformError::Unavailable)
                    },
                    |_| true,
                )
                .unwrap();
            let (shutdown, ()) = Box::pin(network::both(driver, async {
                let end = Instant::now() + Duration::from_secs(1);
                while s.broker.native_status().unwrap().exit.is_none() {
                    assert!(Instant::now() < end);
                    asupersync::time::sleep(s.cx.now(), Duration::from_millis(1)).await;
                }
                assert!(
                    s.broker
                        .service(&mut s.pair.server, Some(s.request.target))
                        .is_err()
                );
                assert!(s.observation.check().is_err());
            }))
            .await;
            assert_eq!(shutdown.handoff_safe(), !panic);
            assert_eq!(s.seat.is_occupied(), panic);
        }
    });
}
#[test]
fn changed_view_and_abandoned_unpolled_io_revoke_without_early_seat_release() {
    run(|cx, second| async move {
        for (abandoned, cx) in [false, true].into_iter().zip([cx, second]) {
            let mut s = Setup::new(
                cx.clone(),
                Seat::default(),
                AuthorityPolicy::plan_defaults(),
                true,
            )
            .await;
            s.request().await;
            let (tx, rx) = mpsc::channel();
            let driver = s
                .broker
                .approve(
                    &s.pair.server,
                    s.request.target,
                    || Some((InputLeaseId::from_raw(20), InputTicketId::from_raw(30))),
                    move || {
                        let _ = rx.recv();
                        Ok(EmptySink)
                    },
                    |_| true,
                )
                .unwrap();
            let (shutdown, ()) = Box::pin(network::both(driver, async {
                if abandoned {
                    drop(s.broker.drive(&mut s.pair.server, Duration::from_millis(1)));
                } else {
                    assert_eq!(
                        s.broker.service(&mut s.pair.server, None),
                        Err(GrantError::TargetChanged)
                    );
                }
                assert!(s.broker.native_status().unwrap().stopped);
                assert!(s.seat.is_occupied());
                assert!(s.pair.server.is_closed());
                tx.send(()).unwrap();
            }))
            .await;
            assert!(shutdown.handoff_safe());
        }
    });
}
#[test]
fn broker_rejects_observe_role_missing_capability_duplicate_owner_and_legacy_feedback() {
    run(|cx, _second| async move {
        let s = Setup::new(cx, Seat::default(), AuthorityPolicy::plan_defaults(), true).await;
        for mode in 0..3 {
            let mut selected = selection();
            if mode == 0 {
                selected.role = SessionRole::Observe;
            }
            if mode == 1 {
                selected.capabilities.clear();
            }
            let result = GrantBroker::new(
                s.observation.clone(),
                &s.pair.server,
                s.seat.clone(),
                Scope {
                    parent: s.request.parent,
                    control: s.pair.control_routes,
                    selection: &selected,
                },
                s.pair.routes,
            );
            let expected = if mode == 2 {
                GrantError::AlreadyAttached
            } else {
                GrantError::NotNegotiated
            };
            assert!(matches!(result,Err(e) if e==expected));
        }
        let old = pair(&s.cx).await;
        assert!(matches!(
            GrantBroker::new(
                s.observation.clone(),
                &old.server,
                s.seat.clone(),
                Scope {
                    parent: s.request.parent,
                    control: old.control_routes,
                    selection: &selection()
                },
                old.routes
            ),
            Err(GrantError::InvalidRoutes)
        ));
        assert!(!s.seat.is_occupied());
        assert!(s.observation.check().is_ok());
    });
}
#[test]
fn reentrant_credential_generation_sees_reserved_seat_without_authority_lock() {
    run(|cx, _second| async move {
        let mut s = Setup::new(cx, Seat::default(), AuthorityPolicy::plan_defaults(), true).await;
        s.request().await;
        let seat = s.seat.clone();
        let observation = s.observation.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let driver = s
            .broker
            .approve(
                &s.pair.server,
                s.request.target,
                move || {
                    assert!(seat.is_occupied());
                    assert!(observation.check().is_ok());
                    c.fetch_add(1, Ordering::Relaxed);
                    Some((InputLeaseId::from_raw(20), InputTicketId::from_raw(30)))
                },
                || Ok(EmptySink),
                |_| true,
            )
            .unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            s.broker.stop();
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

async fn blocked_grant(s: &mut Setup) {
    let end = Instant::now() + Duration::from_secs(1);
    loop {
        assert!(Instant::now() < end);
        match s
            .broker
            .service(&mut s.pair.server, Some(s.request.target))
            .unwrap()
        {
            Event::Backpressure => return,
            Event::NativeStarting => {
                asupersync::time::sleep(s.cx.now(), Duration::from_millis(1)).await;
            }
            other => panic!("expected real grant backpressure, got {other:?}"),
        }
    }
}
#[test]
fn native_grant_backpressure_preserves_issue_time_expiry_and_single_credential() {
    run(|cx, _second| async move {
        let mut s = Setup::new(cx, Seat::default(), AuthorityPolicy::plan_defaults(), true).await;
        s.request().await;
        s.pair
            .server
            .send(
                &s.cx,
                Route::Stream(s.pair.auxiliary),
                &filler(),
                network::clock(&s.cx) + 2_000_000,
                || true,
            )
            .unwrap();
        let before = network::clock(&s.cx);
        let driver = s.approve();
        let after = network::clock(&s.cx);
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            blocked_grant(&mut s).await;
            asupersync::time::sleep(s.cx.now(), Duration::from_millis(110)).await;
            for _ in 0..4 {
                assert_eq!(
                    s.broker
                        .service(&mut s.pair.server, Some(s.request.target))
                        .unwrap(),
                    Event::Backpressure
                );
            }
            // Drain the actual unrelated record/ACK; no grant was enqueued yet.
            let mut received = false;
            let end = Instant::now() + Duration::from_secs(1);
            while !received {
                assert!(Instant::now() < end);
                s.pump().await;
                s.pair
                    .client
                    .receive(
                        &s.cx,
                        || true,
                        |route, b| {
                            assert_eq!(
                                route,
                                Route::Stream(StreamRoute {
                                    outbound: false,
                                    ..s.pair.auxiliary
                                })
                            );
                            assert_eq!(b, filler());
                            received = true;
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
            }
            let (mut f, g) = Box::pin(s.finish()).await;
            assert!((before..=after).contains(&g.issued_at_us));
            assert_eq!(g.ticket_until_us - g.issued_at_us, 1_000_000);
            assert!(network::clock(&f.cx) - g.issued_at_us >= 110_000);
            assert!(f.client.ticket_deadline().unwrap().0 <= g.ticket_until_us);
            assert!(g.ticket_until_us - network::clock(&f.cx) < 900_000);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn unsent_grant_expires_in_place_without_any_replacement_or_native_input() {
    run(|cx, _second| async move {
        let policy = AuthorityPolicy {
            authorization_lifetime: HostDuration::from_millis_checked(600).unwrap(),
            ticket_lifetime: HostDuration::from_millis_checked(120).unwrap(),
        };
        let mut s = Setup::new(cx, Seat::default(), policy, true).await;
        s.request().await;
        s.pair
            .server
            .send(
                &s.cx,
                Route::Stream(s.pair.auxiliary),
                &filler(),
                network::clock(&s.cx) + 1_000_000,
                || true,
            )
            .unwrap();
        let driver = s.approve();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            blocked_grant(&mut s).await;
            asupersync::time::sleep(s.cx.now(), Duration::from_millis(150)).await;
            assert_eq!(
                s.broker.service(&mut s.pair.server, Some(s.request.target)),
                Err(GrantError::Expired)
            );
            assert!(s.broker.native_status().unwrap().stopped);
            assert!(s.pair.server.is_closed());
            assert_eq!(s.observer.query_pointer().unwrap().1 & 0x0101, 0);
        }))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!s.seat.is_occupied());
    });
}
#[test]
fn foreign_connection_with_equal_routes_cannot_receive_grant_or_be_closed() {
    run(|cx, second| async move {
        let mut s = Setup::new(cx, Seat::default(), AuthorityPolicy::plan_defaults(), true).await;
        s.request().await;
        let mut foreign = pair_with_feedback(&second, true).await;
        assert_eq!(
            s.pair.control_routes.outbound,
            foreign.control_routes.outbound
        );
        let before = foreign.server.usage();
        let driver = s.approve();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            assert_eq!(
                s.broker
                    .service(&mut foreign.server, Some(s.request.target)),
                Err(GrantError::WrongConnection)
            );
            assert!(!foreign.server.is_closed());
            assert_eq!(foreign.server.usage(), before);
            assert!(s.observation.check().is_err());
            assert!(s.broker.native_status().unwrap().stopped);
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
async fn deliver_unapproved(s: &mut Setup, route: Route, bytes: &[u8]) -> GrantError {
    let until = network::clock(&s.cx) + 1_000_000;
    let end = Instant::now() + Duration::from_secs(1);
    loop {
        assert!(Instant::now() < end);
        match s.pair.client.send(&s.cx, route, bytes, until, || true) {
            Ok(()) => break,
            Err(quic::Error::Backpressure) => s.pump().await,
            other => panic!("unexpected input send {other:?}"),
        }
    }
    loop {
        assert!(Instant::now() < end);
        s.pump().await;
        if let Err(error) = s.broker.receive(&mut s.pair.server, |_, _| {
            panic!("unexpected unapproved record")
        }) {
            return error;
        }
    }
}
#[test]
fn denied_request_replay_and_early_input_never_create_a_native_owner() {
    run(|cx, second| async move {
        let mut a = Setup::new(cx, Seat::default(), AuthorityPolicy::plan_defaults(), true).await;
        a.request().await;
        a.broker.deny();
        let mut bytes = [0; fr_wire::control::REQUEST_BYTES];
        fr_wire::control::encode_request(
            a.request,
            &mut bytes,
            &ProtocolLimits::ABSOLUTE,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        let route = Route::Stream(StreamRoute {
            outbound: true,
            ..a.pair.control_routes.inbound
        });
        assert_eq!(
            deliver_unapproved(&mut a, route, &bytes).await,
            GrantError::Replay
        );
        assert!(!a.seat.is_occupied());
        let mut b = Setup::new(
            second,
            Seat::default(),
            AuthorityPolicy::plan_defaults(),
            true,
        )
        .await;
        b.request().await;
        let req = InputRequest {
            credentials: credentials(),
            sequence: 0,
            event: InputEvent::Key {
                key: PhysicalKey::new(225).unwrap(),
                transition: KeyTransition::Press,
            },
        };
        let mut bytes = [0; 256];
        let n = encode_input(
            req,
            &mut bytes,
            &ProtocolLimits::ABSOLUTE,
            7,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        let route = Route::Stream(b.pair.actions);
        assert_eq!(
            deliver_unapproved(&mut b, route, &bytes[..n]).await,
            GrantError::NativeNotReady
        );
        assert!(!b.seat.is_occupied());
        assert_eq!(b.observer.query_pointer().unwrap().1 & 1, 0);
    });
}

#[test]
fn late_local_denial_revokes_a_pending_native_grant() {
    run(|cx, _second| async move {
        let mut s = Setup::new(cx, Seat::default(), AuthorityPolicy::plan_defaults(), true).await;
        s.request().await;
        let (tx, rx) = mpsc::channel();
        let driver = s
            .broker
            .approve(
                &s.pair.server,
                s.request.target,
                || Some((InputLeaseId::from_raw(20), InputTicketId::from_raw(30))),
                move || {
                    let _ = rx.recv();
                    Ok(EmptySink)
                },
                |_| true,
            )
            .unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            s.broker.deny();
            assert!(s.broker.native_status().unwrap().stopped);
            assert!(s.seat.is_occupied());
            assert!(s.observation.check().is_err());
            assert_eq!(
                s.broker.service(&mut s.pair.server, Some(s.request.target)),
                Err(GrantError::Stopped)
            );
            tx.send(()).unwrap();
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn publication_rejects_already_buffered_input_without_requiring_another_receive_call() {
    run(|cx, _second| async move {
        let mut s = Setup::new(cx, Seat::default(), AuthorityPolicy::plan_defaults(), true).await;
        s.request().await;
        let (tx, rx) = mpsc::channel();
        let driver = s
            .broker
            .approve(
                &s.pair.server,
                s.request.target,
                || Some((InputLeaseId::from_raw(20), InputTicketId::from_raw(30))),
                move || {
                    let _ = rx.recv();
                    Ok(EmptySink)
                },
                |_| true,
            )
            .unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let req = InputRequest {
                credentials: credentials(),
                sequence: 0,
                event: InputEvent::Key {
                    key: PhysicalKey::new(225).unwrap(),
                    transition: KeyTransition::Press,
                },
            };
            let mut bytes = [0; 256];
            let n = encode_input(
                req,
                &mut bytes,
                &ProtocolLimits::ABSOLUTE,
                7,
                InputDirection::ViewerToHost,
                InputDelivery::Reliable,
            )
            .unwrap();
            let until = network::clock(&s.cx) + 1_000_000;
            loop {
                match s.pair.client.send(
                    &s.cx,
                    Route::Stream(s.pair.actions),
                    &bytes[..n],
                    until,
                    || true,
                ) {
                    Ok(()) => break,
                    Err(quic::Error::Backpressure) => s.pump().await,
                    other => panic!("input send {other:?}"),
                }
            }
            let end = Instant::now() + Duration::from_secs(1);
            loop {
                assert!(Instant::now() < end);
                s.pump().await;
                match s.broker.service(&mut s.pair.server, Some(s.request.target)) {
                    Ok(Event::NativeStarting) => {}
                    Err(GrantError::NativeNotReady) => break,
                    other => panic!("early input must stop grant {other:?}"),
                }
            }
            assert!(s.broker.native_status().unwrap().stopped);
            assert!(s.seat.is_occupied());
            tx.send(()).unwrap();
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[path = "broker/negotiated.rs"]
mod negotiated_grant;
