//! Actual X11 state, native owner, and UDP/TLS. Local grants remain fixtures.
use super::*;
use fr_core::{
    held_state::{HeldState, HeldStateRequest},
    input_submission::{Reconciliation, ReconciliationOutcome},
};
use fr_wire::held_state::{HELD_STATE_BYTES, encode};
use frd::input_agent::Reply;
fn snapshot(sequence: u64, next_action: u64, held: HeldState) -> [u8; HELD_STATE_BYTES] {
    let c = credentials();
    let mut out = [0; HELD_STATE_BYTES];
    encode(
        HeldStateRequest {
            session: c.session,
            lease: c.lease,
            sequence,
            next_action,
            held,
        },
        &mut out,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    out
}
async fn reconciled(f: &mut Fixture, sequence: u64) -> Reconciliation {
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        assert!(Instant::now() < end, "native reconciliation did not finish");
        if let Progress::Reconciliation(Reply::Reconciliation(Ok(report))) = f.turn().await {
            assert_eq!(report.sequence, sequence);
            return report;
        }
    }
}
async fn pressed(f: &mut Fixture) {
    let bytes = f.action(shift(KeyTransition::Press));
    f.send(&bytes, Route::Stream(f.pair.actions));
    f.until_receipts(1).await;
    let bytes = f.action(button(true));
    f.send(&bytes, Route::Stream(f.pair.actions));
    f.until_receipts(2).await;
    assert_eq!(f.observer.query_pointer().unwrap().1 & (1 | 256), 257);
}
#[test]
fn missed_local_release_reconciles_real_modifier_and_drag_then_accepts_new_press() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            pressed(&mut f).await;
            let mut out = [0; HELD_STATE_BYTES];
            let r = f
                .client
                .reconcile_held(
                    HeldState::empty(),
                    &mut out,
                    ClientInstant(network::clock(&f.cx)),
                )
                .unwrap()
                .unwrap();
            assert_eq!(r.next_action, 2);
            f.send(&out, Route::Stream(f.pair.actions));
            let report = reconciled(&mut f, 0).await;
            assert_eq!(report.outcome, ReconciliationOutcome::Applied);
            assert_eq!(report.submitted_releases, 2);
            assert_eq!(f.observer.query_pointer().unwrap().1 & (1 | 256), 0);
            assert!(!f.input.control().is_stopped());
            assert_eq!(f.receipts.len(), 2);
            for action in [shift(KeyTransition::Press), button(true)] {
                let bytes = f.action(action);
                let n = f.receipts.len() + 1;
                f.send(&bytes, Route::Stream(f.pair.actions));
                f.until_receipts(n).await;
            }
            assert_eq!(f.receipts[2].sequence, 2);
            assert_eq!(f.receipts[3].sequence, 3);
            assert_eq!(f.observer.query_pointer().unwrap().1 & (1 | 256), 257);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn snapshot_preserves_real_held_modifier_and_never_synthesizes_requested_buttons() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            pressed(&mut f).await;
            let mut held = HeldState::empty();
            held.set_key(PhysicalKey::new(225).unwrap(), true);
            held.set_key(PhysicalKey::new(226).unwrap(), true);
            held.set_button(PointerButton::Secondary, true);
            f.send(&snapshot(0, 2, held), Route::Stream(f.pair.actions));
            let report = reconciled(&mut f, 0).await;
            assert_eq!(report.submitted_releases, 1);
            assert_eq!(f.observer.query_pointer().unwrap().1, 1);
            f.send(
                &snapshot(1, 2, HeldState::empty()),
                Route::Stream(f.pair.actions),
            );
            assert_eq!(reconciled(&mut f, 1).await.submitted_releases, 1);
            assert_eq!(f.observer.query_pointer().unwrap().1, 0);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn obsolete_action_position_foreign_lease_and_duplicate_snapshot_cannot_release_new_drag() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            pressed(&mut f).await;
            f.send(
                &snapshot(100, 0, HeldState::empty()),
                Route::Stream(f.pair.actions),
            );
            assert_eq!(
                reconciled(&mut f, 100).await.outcome,
                ReconciliationOutcome::Ignored
            );
            let mut foreign = snapshot(101, 2, HeldState::empty());
            foreign[55] = 9;
            f.send(&foreign, Route::Stream(f.pair.actions));
            assert_eq!(
                reconciled(&mut f, 101).await.outcome,
                ReconciliationOutcome::Ignored
            );
            assert_eq!(f.observer.query_pointer().unwrap().1, 257);
            let mut bytes = [0; HELD_STATE_BYTES];
            let sent = f
                .client
                .reconcile_held(
                    HeldState::empty(),
                    &mut bytes,
                    ClientInstant(network::clock(&f.cx)),
                )
                .unwrap()
                .unwrap();
            assert_eq!(sent.next_action, 2);
            f.send(&bytes, Route::Stream(f.pair.actions));
            assert_eq!(reconciled(&mut f, 0).await.submitted_releases, 2);
            let bytes = f.action(button(true));
            f.send(&bytes, Route::Stream(f.pair.actions));
            f.until_receipts(3).await;
            assert_eq!(f.receipts[2].sequence, 2);
            f.send(
                &snapshot(0, 3, HeldState::empty()),
                Route::Stream(f.pair.actions),
            );
            assert_eq!(
                reconciled(&mut f, 0).await.outcome,
                ReconciliationOutcome::Ignored
            );
            assert_eq!(f.observer.query_pointer().unwrap().1 & 256, 256);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn reconciliation_waits_behind_the_exact_pending_action_receipt() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let b = f.action(button(true));
            f.send(&b, Route::Stream(f.pair.actions));
            while !f.input.status().outstanding {
                f.io().await;
                f.receive();
            }
            // Buffer the snapshot in actual QUIC while deliberately leaving the
            // native action result uncollected. Both records share one stream.
            let state = snapshot(0, 1, HeldState::empty());
            let deadline = network::clock(&f.cx) + 1_000_000;
            loop {
                assert!(network::clock(&f.cx) < deadline);
                match f.pair.client.send(
                    &f.cx,
                    Route::Stream(f.pair.actions),
                    &state,
                    deadline,
                    || true,
                ) {
                    Ok(()) => break,
                    Err(quic::Error::Backpressure) => f.io().await,
                    other => panic!("snapshot enqueue failed: {other:?}"),
                }
            }
            for _ in 0..8 {
                f.io().await;
            }
            f.pair
                .server
                .send(
                    &f.cx,
                    Route::Stream(f.pair.auxiliary),
                    &filler(),
                    network::clock(&f.cx) + 1_000_000,
                    || true,
                )
                .unwrap();
            let until = Instant::now() + Duration::from_secs(1);
            while f.input.pending_receipt().is_none() {
                assert!(Instant::now() < until);
                f.input.service(&mut f.pair.server, || true).unwrap();
                asupersync::time::sleep(f.cx.now(), Duration::from_millis(1)).await;
            }
            let saved = f.input.pending_receipt().unwrap();
            for _ in 0..8 {
                assert_eq!(
                    f.input.service(&mut f.pair.server, || true).unwrap(),
                    Progress::ReceiptBackpressure
                );
                assert_eq!(
                    f.input
                        .receive(&mut f.pair.server, || true, |_| false, |_, _| panic!())
                        .unwrap(),
                    0
                );
                assert_eq!(f.input.pending_receipt(), Some(saved));
                assert!(f.input.last_reconciliation().is_none());
                assert_eq!(f.observer.query_pointer().unwrap().1 & 256, 256);
            }
            let report = reconciled(&mut f, 0).await;
            assert_eq!(report.outcome, ReconciliationOutcome::Applied);
            assert_eq!(report.submitted_releases, 1);
            f.until_receipts(1).await;
            assert_eq!(f.receipts, vec![saved]);
            assert_eq!(f.observer.query_pointer().unwrap().1 & 256, 0);
            assert!(!f.input.control().is_stopped());
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn malformed_held_state_revokes_then_cleans_real_held_input_without_an_action_receipt() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            pressed(&mut f).await;
            let mut malformed = snapshot(0, 2, HeldState::empty());
            malformed[104] = 0x80; // An undefined button, not an empty state.
            f.send(&malformed, Route::Stream(f.pair.actions));
            let until = Instant::now() + Duration::from_secs(1);
            loop {
                assert!(Instant::now() < until);
                f.io().await;
                match f
                    .input
                    .receive(&mut f.pair.server, || true, |_| false, |_, _| panic!())
                {
                    Err(Error::Agent(frd::input_agent::Error::Wire(_))) => break,
                    Ok(_) => {}
                    other => panic!("unexpected malformed-state result: {other:?}"),
                }
            }
            assert!(f.pair.server.is_closed());
            assert!(f.input.control().is_stopped());
            assert!(f.input.pending_receipt().is_none());
            assert!(f.input.last_reconciliation().is_none());
            f.cleared().await;
            assert_eq!(f.receipts.len(), 2);
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn expired_input_ticket_does_not_prevent_live_lease_release_reconciliation() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new(cx).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            pressed(&mut f).await;
            // Plan-default input tickets last one second; the control lease
            // lasts three. This is release-only cleanup, not stale input.
            asupersync::time::sleep(f.cx.now(), Duration::from_millis(1100)).await;
            assert!(!f.input.control().is_stopped());
            assert_eq!(f.observer.query_pointer().unwrap().1 & 0x0101, 257);
            f.send(
                &snapshot(0, 2, HeldState::empty()),
                Route::Stream(f.pair.actions),
            );
            let report = reconciled(&mut f, 0).await;
            assert_eq!(report.outcome, ReconciliationOutcome::Applied);
            assert_eq!(report.submitted_releases, 2);
            assert_eq!(f.observer.query_pointer().unwrap().1 & 0x0101, 0);
            assert!(!f.input.control().is_stopped());
            assert_eq!(f.receipts.len(), 2);
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
