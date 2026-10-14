//! Real existing UDP/TLS and XTest fixtures with explicit scheduling faults.
//! Never-admitted tickets are disposable; authority loss and effect receipts
//! keep their terminal behavior. No live-tailnet or impairment claim.
use super::*;
use frd::input_agent::InputReply;

#[test]
fn ticket_expiry_during_the_transport_admission_check_does_not_revoke_control() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx, true).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            occupy_feedback(&mut f);
            renew(&mut f, 10);
            let saved = collect_blocked_ticket(&mut f).await;
            // Make real transport credit available WITHOUT servicing the
            // pending ticket: it still belongs exclusively to QuicInput.
            while f.pair.server.usage().critical_send_records != 0 {
                assert!(network::clock(&f.cx) < saved.expires_at_us);
                f.io().await;
            }
            assert!(network::clock(&f.cx) < saved.expires_at_us);
            assert_eq!(f.input.pending_ticket(), Some(saved));
            let clock = f.cx.clone();
            let mut calls = 0;
            let result = f.input.service(&mut f.pair.server, || {
                calls += 1;
                if calls == 2 {
                    // Deliberately model preemption inside the second gate,
                    // after service's expiry sample but before QUIC admission.
                    // Production authority callbacks must not block like this.
                    let delay = saved.expires_at_us.saturating_sub(network::clock(&clock)) + 1_000;
                    std::thread::sleep(Duration::from_micros(delay));
                }
                true
            });
            assert_eq!(calls, 2, "must exercise the final transport path");
            assert_eq!(result, Ok(Progress::Idle));
            assert!(f.input.pending_ticket().is_none());
            assert!(f.input.pending_receipt().is_none());
            assert!(!f.input.control().is_stopped());
            assert!(!f.pair.server.is_closed());
            assert_eq!(f.pair.server.usage().critical_send_records, 0);
            assert!(f.tickets.is_empty());
            f.input.control().stop(StopReason::LocalRevoke);
            f.cleared().await;
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn expired_ticket_disposal_cannot_hide_local_revoke_or_connection_denial() {
    for revoke in [false, true] {
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
                if revoke {
                    f.input.control().stop(StopReason::LocalRevoke);
                }
                let result = f.input.service(&mut f.pair.server, || revoke);
                assert_eq!(
                    result,
                    Err(if revoke {
                        Error::Closed
                    } else {
                        Error::Transport(quic::Error::Unauthorized)
                    })
                );
                assert!(f.input.control().is_stopped());
                assert!(f.pair.server.is_closed());
                assert!(f.tickets.is_empty());
                assert!(
                    f.input
                        .renew_ticket(
                            &mut f.pair.server,
                            || true,
                            || panic!("terminal owner cannot issue another ticket"),
                        )
                        .is_err()
                );
                f.cleared().await;
            }))
            .await;
            assert!(shutdown.handoff_safe());
        });
    }
}

#[test]
fn an_expired_action_receipt_is_retained_and_still_ends_the_connection() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut f = Fixture::new_options(cx, true).await;
        let driver = f.driver.take().unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let bytes = f.action(button(true));
            f.send(&bytes, Route::Stream(f.pair.actions));
            let until = Instant::now() + Duration::from_secs(1);
            while !f.input.status().outstanding {
                assert!(Instant::now() < until);
                f.io().await;
                f.receive();
            }
            occupy_feedback(&mut f);
            while f.input.pending_receipt().is_none() {
                assert!(Instant::now() < until);
                f.input.service(&mut f.pair.server, || true).unwrap();
                asupersync::time::sleep(f.cx.now(), Duration::from_millis(1)).await;
            }
            let saved = f.input.pending_receipt().unwrap();
            assert_eq!(saved.outcome, InputOutcome::SubmittedToOs);
            assert!(saved.submitted_operations > 0);
            assert_eq!(
                f.input
                    .renew_ticket(
                        &mut f.pair.server,
                        || true,
                        || panic!("an effect receipt retains priority"),
                    )
                    .unwrap(),
                Progress::ReceiptBackpressure
            );
            // The receipt's fixed 1s admission lifetime is distinct from the
            // filler record's 2s lifetime; only the receipt expires here.
            asupersync::time::sleep(f.cx.now(), Duration::from_millis(1_050)).await;
            assert_eq!(
                f.input.service(&mut f.pair.server, || true),
                Err(Error::Transport(quic::Error::Expired))
            );
            assert!(f.input.control().is_stopped());
            assert!(f.pair.server.is_closed());
            assert_eq!(f.input.pending_receipt(), Some(saved));
            assert_eq!(f.input.last_reply(), Some(InputReply::Record(saved)));
            assert!(f.input.pending_ticket().is_none());
            assert!(f.tickets.is_empty());
            f.cleared().await;
            // Releasing held input is cleanup, not rollback of the receipt.
            assert_eq!(f.input.pending_receipt(), Some(saved));
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
