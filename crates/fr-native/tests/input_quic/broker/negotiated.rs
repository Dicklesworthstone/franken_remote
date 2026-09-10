//! Initial control with actual ticket-negotiated configuration and input routes.
//! Local approval, readiness and same-runtime clock correlation are explicit
//! fixtures. No preinstalled input routes or pre-granted lease are used.
use super::*;
use frd::input_quic::NegotiatedInput;

fn has_no_control(s: &Setup) -> bool {
    s.observation
        .input_session(
            InputCredentials {
                session: s.request.parent.remote_session,
                lease: InputLeaseId::from_raw(20),
                ticket: InputTicketId::from_raw(30),
                view: s.request.target.view,
            },
            s.request.target.bounds,
            s.request.target.capabilities,
        )
        .is_err()
}

async fn receipts(f: &mut Fixture, viewer: &NegotiatedInput, count: usize) {
    let until = Instant::now() + Duration::from_secs(2);
    while f.receipts.len() < count {
        assert!(Instant::now() < until, "negotiated grant receipt missing");
        f.turn_with(Some(viewer)).await;
    }
}
fn send(f: &mut Fixture, viewer: &NegotiatedInput, bytes: &[u8]) {
    viewer
        .send(
            &f.cx,
            &mut f.pair.client,
            bytes,
            network::clock(&f.cx) + 1_000_000,
            || true,
        )
        .unwrap();
}
#[test]
fn negotiated_broker_initial_grant_executes_native_drag_and_renews_the_same_owner() {
    run(|cx, _| async move {
        let (mut s, viewer) =
            Setup::negotiated(cx, Seat::default(), AuthorityPolicy::plan_defaults(), true).await;
        assert!(has_no_control(&s));
        assert!(!s.seat.is_occupied());
        assert!(s.broker.native_status().is_none());
        s.request().await;
        assert!(has_no_control(&s), "request is not consent");
        let driver = s.approve();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let (mut f, g) = Box::pin(s.finish()).await;
            assert_eq!(g.request.parent, super::super::negotiated::parent());
            assert_eq!(g.request.target.display_binding, 6);
            assert_eq!(g.input_channel, viewer.channel_binding());
            for (i, action) in [shift(KeyTransition::Press), button(true)]
                .into_iter()
                .enumerate()
            {
                let bytes = f.action(action);
                send(&mut f, &viewer, &bytes);
                receipts(&mut f, &viewer, i + 1).await;
                assert_eq!(f.receipts[i].outcome, InputOutcome::SubmittedToOs);
            }
            assert_eq!(f.observer.query_pointer().unwrap().1 & 0x0101, 257);
            let mut bytes = [0; MAX_INPUT_RECORD_BYTES];
            let n = f
                .client
                .pointer(
                    DesktopPoint { x: 101, y: 102 },
                    &mut bytes,
                    ClientInstant(network::clock(&f.cx)),
                )
                .unwrap();
            send(&mut f, &viewer, &bytes[..n.bytes]);
            receipts(&mut f, &viewer, 3).await;
            assert_eq!(
                f.observer.query_pointer().unwrap(),
                (DesktopPoint { x: 101, y: 102 }, 257)
            );
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
                f.turn_with(Some(&viewer)).await;
            }
            assert_eq!(f.tickets[0].sequence, 1);
            assert_eq!(f.tickets[0].credentials.lease, g.lease);
            for action in [button(false), shift(KeyTransition::Release)] {
                let n = f.receipts.len() + 1;
                let bytes = f.action(action);
                send(&mut f, &viewer, &bytes);
                receipts(&mut f, &viewer, n).await;
                assert_eq!(f.receipts[n - 1].outcome, InputOutcome::SubmittedToOs);
            }
            assert_eq!(f.client.pending_actions(), 0);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

async fn forged_request(s: &mut Setup, request: Request) -> GrantError {
    let mut bytes = [0; fr_wire::control::REQUEST_BYTES];
    let n = fr_wire::control::encode_request(
        request,
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    let deadline = network::clock(&s.cx) + 1_000_000;
    loop {
        assert!(network::clock(&s.cx) < deadline);
        match s.pair.client.send(
            &s.cx,
            Route::Stream(StreamRoute {
                outbound: true,
                ..s.pair.control_routes.inbound
            }),
            &bytes[..n],
            deadline,
            || true,
        ) {
            Ok(()) => break,
            Err(quic::Error::Backpressure) => s.pump().await,
            other => panic!("forged request was not admitted: {other:?}"),
        }
    }
    loop {
        assert!(network::clock(&s.cx) < deadline);
        s.pump().await;
        match s
            .broker
            .receive(&mut s.pair.server, |_, _| panic!("unexpected record"))
        {
            Err(e) => return e,
            Ok(_) => assert!(
                s.broker.request().is_none(),
                "foreign view accepted by broker"
            ),
        }
    }
}
#[test]
fn negotiated_broker_rejects_foreign_display_and_every_view_generation_before_approval() {
    for dimension in 0..5 {
        run(|cx, _| async move {
            let (mut s, viewer) = Setup::negotiated(
                cx.clone(),
                Seat::default(),
                AuthorityPolicy::plan_defaults(),
                true,
            )
            .await;
            let mut request = s.request;
            match dimension {
                0 => request.target.display_binding = 12,
                1 => request.target.view.geometry = request.target.view.geometry.next().unwrap(),
                2 => request.target.view.viewport = request.target.view.viewport.next().unwrap(),
                3 => {
                    request.target.view.configuration =
                        request.target.view.configuration.next().unwrap();
                }
                _ => request.target.view.recovery = request.target.view.recovery.next().unwrap(),
            }
            assert!(viewer.check_request(&s.pair.client, request).is_err());
            assert_eq!(
                forged_request(&mut s, request).await,
                GrantError::TargetChanged
            );
            assert!(s.broker.request().is_none());
            assert!(s.broker.native_status().is_none());
            assert!(!s.seat.is_occupied());
            assert!(s.observation.check().is_err());
            assert_eq!(s.observer.query_pointer().unwrap().1 & 0x0101, 0);
        });
    }
}
#[test]
fn negotiated_channels_do_not_supply_consent_or_readiness() {
    run(|cx, _| async move {
        let (mut s, _) =
            Setup::negotiated(cx, Seat::default(), AuthorityPolicy::plan_defaults(), false).await;
        s.request().await;
        assert_eq!(
            s.broker.service(&mut s.pair.server, None).unwrap(),
            Event::AwaitingApproval
        );
        assert!(s.broker.native_status().is_none());
        assert!(has_no_control(&s));
        let result = s.broker.approve::<X11Pointer, _, _>(
            &s.pair.server,
            s.request.target,
            || Some((InputLeaseId::from_raw(20), InputTicketId::from_raw(30))),
            || panic!("negotiation cannot create native readiness"),
            X11Pointer::cleanup_native,
        );
        assert!(matches!(result, Err(GrantError::Media(_))));
        assert!(!s.seat.is_occupied());
        assert!(s.observation.check().is_ok());
    });
}
#[test]
fn negotiated_contenders_still_share_one_seat_before_generating_any_credential() {
    run(|cx, second| async move {
        let seat = Seat::default();
        let (mut a, _) =
            Setup::negotiated(cx, seat.clone(), AuthorityPolicy::plan_defaults(), true).await;
        let (mut b, _) =
            Setup::negotiated(second, seat.clone(), AuthorityPolicy::plan_defaults(), true).await;
        a.request().await;
        b.request().await;
        let driver = a.approve();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            assert!(matches!(
                b.broker.approve::<X11Pointer, _, _>(
                    &b.pair.server,
                    b.request.target,
                    || panic!("competing grant generated credentials"),
                    || panic!("competing grant opened native input"),
                    X11Pointer::cleanup_native,
                ),
                Err(GrantError::Agent(frd::input_agent::Error::SeatBusy))
            ));
            assert!(b.observation.check().is_ok());
            assert!(has_no_control(&b));
            a.broker.stop();
        }))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
        let driver = b.approve();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            b.queued().await;
            b.broker.stop();
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn negotiated_broker_abandonment_revokes_during_native_initialization() {
    run(|cx, _| async move {
        let (mut s, _) =
            Setup::negotiated(cx, Seat::default(), AuthorityPolicy::plan_defaults(), true).await;
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
                    rx.recv_timeout(Duration::from_secs(3)).unwrap();
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
            drop(s.broker.drive(&mut s.pair.server, Duration::ZERO));
            assert!(s.pair.server.is_closed());
            assert!(s.observation.check().is_err());
            assert!(
                s.seat.is_occupied(),
                "blocked native initialization still owns seat"
            );
            tx.send(()).unwrap();
        }))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!s.seat.is_occupied());
    });
}
