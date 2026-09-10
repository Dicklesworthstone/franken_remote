//! Native owner, actual UDP/TLS and XKB/XTest. Consent/view/clock are explicit
//! same-runtime fixtures. Every case waits for native cleanup before server exit.
use super::*;
use fr_wire::input_ticket::{INPUT_TICKET_BYTES, Ticket};

fn renew(f: &mut Fixture, id: u128) -> Progress {
    f.input
        .renew_ticket(
            &mut f.pair.server,
            || true,
            || Some(InputTicketId::from_raw(id)),
        )
        .unwrap()
}
async fn until_tickets(f: &mut Fixture, n: usize) {
    let end = Instant::now() + Duration::from_secs(2);
    while f.tickets.len() < n {
        assert!(Instant::now() < end, "ticket did not arrive");
        f.turn().await;
    }
}
fn occupy_feedback(f: &mut Fixture) {
    f.pair
        .server
        .send(
            &f.cx,
            Route::Stream(f.pair.auxiliary),
            &filler(),
            network::clock(&f.cx) + 2_000_000,
            || true,
        )
        .unwrap();
}
async fn collect_blocked_ticket(f: &mut Fixture) -> Ticket {
    let end = Instant::now() + Duration::from_secs(1);
    while f.input.pending_ticket().is_none() {
        assert!(Instant::now() < end);
        let p = f.input.service(&mut f.pair.server, || true).unwrap();
        assert!(matches!(
            p,
            Progress::NativePending | Progress::TicketBackpressure
        ));
        asupersync::time::sleep(f.cx.now(), Duration::from_millis(1)).await;
    }
    f.input.pending_ticket().unwrap()
}

#[test]
fn native_rollover_preserves_old_in_flight_press_and_updates_only_future_actions() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx, true).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let old = f.action(shift(KeyTransition::Press));
            assert_eq!(renew(&mut f, 10), Progress::NativePending);
            until_tickets(&mut f, 1).await;
            assert_eq!(f.tickets[0].sequence, 0);
            assert_eq!(f.client.pending_actions(), 1);
            assert!(f.client.ticket_deadline().unwrap().0 <= f.tickets[0].expires_at_us);
            f.send(&old, Route::Stream(f.pair.actions));
            f.until_receipts(1).await;
            assert_eq!(f.receipts[0].outcome, InputOutcome::SubmittedToOs);
            assert_eq!(f.observer.query_pointer().unwrap().1 & 1, 1);
            let release = f.action(shift(KeyTransition::Release));
            let decoded = fr_wire::input::decode_input(
                &release,
                &ProtocolLimits::ABSOLUTE,
                7,
                InputDirection::ViewerToHost,
                InputDelivery::Reliable,
            )
            .unwrap();
            assert_eq!(decoded.credentials.ticket, InputTicketId::from_raw(10));
            assert_eq!(decoded.sequence, 1);
            f.send(&release, Route::Stream(f.pair.actions));
            f.until_receipts(2).await;
            assert_eq!(f.observer.query_pointer().unwrap().1 & 1, 0);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn renewed_tickets_support_real_drag_and_release_after_initial_ticket_expires() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx, true).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let start = network::clock(&f.cx);
            for i in 0..4u64 {
                while network::clock(&f.cx) < start + i * 280_000 {
                    f.turn().await;
                }
                assert_eq!(renew(&mut f, u128::from(i) + 10), Progress::NativePending);
                until_tickets(&mut f, usize::try_from(i).unwrap() + 1).await;
            }
            while network::clock(&f.cx) < start + 1_050_000 {
                f.turn().await;
            }
            for action in [
                shift(KeyTransition::Press),
                button(true),
                button(false),
                shift(KeyTransition::Release),
            ] {
                let b = f.action(action);
                let n = f.receipts.len() + 1;
                f.send(&b, Route::Stream(f.pair.actions));
                f.until_receipts(n).await;
                assert_eq!(f.receipts[n - 1].outcome, InputOutcome::SubmittedToOs);
            }
            assert_eq!(f.tickets.len(), 4);
            assert!(
                f.tickets
                    .windows(2)
                    .all(|t| t[0].sequence + 1 == t[1].sequence)
            );
            assert_eq!(f.client.pending_actions(), 0);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn send_backpressure_preserves_exact_ticket_and_original_native_expiry() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx, true).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            occupy_feedback(&mut f);
            let before = network::clock(&f.cx);
            renew(&mut f, 10);
            let saved = collect_blocked_ticket(&mut f).await;
            assert!(saved.issued_at_us >= before);
            assert_eq!(saved.expires_at_us - saved.issued_at_us, 1_000_000);
            asupersync::time::sleep(f.cx.now(), Duration::from_millis(280)).await;
            for _ in 0..8 {
                assert_eq!(
                    f.input
                        .renew_ticket(
                            &mut f.pair.server,
                            || true,
                            || panic!("pending renewal must not mint another ID")
                        )
                        .unwrap(),
                    Progress::TicketBackpressure
                );
                assert_eq!(
                    f.input.service(&mut f.pair.server, || true).unwrap(),
                    Progress::TicketBackpressure
                );
                assert_eq!(f.input.pending_ticket(), Some(saved));
                assert!(f.input.pending_receipt().is_none());
                assert!(!f.input.can_accept_input());
            }
            until_tickets(&mut f, 1).await;
            assert_eq!(f.tickets, vec![saved]);
            let now = network::clock(&f.cx);
            assert!(f.client.ticket_deadline().unwrap().0 <= saved.expires_at_us);
            assert!(f.client.ticket_deadline().unwrap().0 - now < 800_000);
            assert_eq!(f.receipts.len(), 0);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn unsent_ticket_expires_in_place_and_revokes_without_minting_replacement() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx, true).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            occupy_feedback(&mut f);
            renew(&mut f, 10);
            let saved = collect_blocked_ticket(&mut f).await;
            let delay = saved.expires_at_us.saturating_sub(network::clock(&f.cx)) + 1_000;
            asupersync::time::sleep(f.cx.now(), Duration::from_micros(delay)).await;
            assert_eq!(
                f.input.service(&mut f.pair.server, || true),
                Err(Error::TicketExpired)
            );
            assert!(f.input.control().is_stopped());
            assert!(f.pair.server.is_closed());
            assert!(f.input.pending_ticket().is_none());
            assert_eq!(f.tickets, []);
            assert!(
                f.input
                    .renew_ticket(
                        &mut f.pair.server,
                        || true,
                        || panic!("closed session must not mint")
                    )
                    .is_err()
            );
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn pending_action_receipt_has_priority_and_is_not_relabelled_as_ticket() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx, true).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let b = f.action(button(true));
            f.send(&b, Route::Stream(f.pair.actions));
            let end = Instant::now() + Duration::from_secs(1);
            while !f.input.status().outstanding {
                assert!(Instant::now() < end);
                f.io().await;
                f.receive();
            }
            occupy_feedback(&mut f);
            while f.input.pending_receipt().is_none() {
                assert!(Instant::now() < end);
                f.input.service(&mut f.pair.server, || true).unwrap();
                asupersync::time::sleep(f.cx.now(), Duration::from_millis(1)).await;
            }
            let saved = f.input.pending_receipt().unwrap();
            assert_eq!(
                f.input
                    .renew_ticket(
                        &mut f.pair.server,
                        || true,
                        || panic!("receipt owns the slot")
                    )
                    .unwrap(),
                Progress::ReceiptBackpressure
            );
            assert_eq!(f.input.pending_receipt(), Some(saved));
            assert!(f.input.pending_ticket().is_none());
            f.until_receipts(1).await;
            assert_eq!(f.receipts, vec![saved]);
            renew(&mut f, 10);
            until_tickets(&mut f, 1).await;
            assert_eq!(f.receipts, vec![saved]);
            assert_eq!(f.observer.query_pointer().unwrap().1 & 256, 256);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn abandoning_network_drive_cannot_deliver_retained_ticket_or_hold_drag() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx, true).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let b = f.action(button(true));
            f.send(&b, Route::Stream(f.pair.actions));
            f.until_receipts(1).await;
            // Drain the prior result's transport retention before occupying it.
            for _ in 0..8 {
                f.io().await;
            }
            occupy_feedback(&mut f);
            renew(&mut f, 10);
            collect_blocked_ticket(&mut f).await;
            drop(
                f.input
                    .drive(&mut f.pair.server, Duration::from_millis(1), || true),
            );
            assert!(f.input.control().is_stopped());
            assert!(f.pair.server.is_closed());
            assert!(f.input.service(&mut f.pair.server, || true).is_err());
            assert_eq!(f.tickets, []);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn legacy_route_refuses_ticket_renewal_and_feedback_is_not_a_wildcard() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx.clone()).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            assert_eq!(
                f.input
                    .renew_ticket(&mut f.pair.server, || true, || panic!("not negotiated")),
                Err(Error::InvalidRoutes)
            );
            assert!(!f.input.control().is_stopped());
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
        let mut p = pair_with_feedback(&cx, true).await;
        assert_eq!(p.routes.results().maximum, INPUT_TICKET_BYTES);
        for kind in [0x0015u16, 0x0040, 0x0042, 0x0046, 0xffff] {
            let mut b = filler();
            b[6..8].copy_from_slice(&kind.to_be_bytes());
            b[16..20].copy_from_slice(&7u32.to_be_bytes());
            assert_eq!(
                p.server.send(
                    &cx,
                    Route::Stream(p.routes.results()),
                    &b,
                    network::clock(&cx) + 1_000_000,
                    || true
                ),
                Err(quic::Error::WrongRoute)
            );
        }
    });
}
