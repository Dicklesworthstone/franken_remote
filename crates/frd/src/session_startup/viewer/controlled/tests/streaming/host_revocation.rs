//! Actual host terminal emission through the real controlled viewer. The native
//! input sink and decoded picture remain the existing explicit test fixtures.
use super::*;
use crate::input_watchdog::StopReason as HostStop;
use fr_wire::lease_revoked::{CleanupStage, EffectStage, Reason};

async fn settle(fixture: &mut Fixture, host_cx: &Cx) {
    let mut nonces = 20_000;
    let mut tickets = 30_000;
    for _ in 0..32 {
        let (host, viewer) = Box::pin(support::both(
            fixture.host.drive(
                Duration::from_millis(1),
                || nonce(&mut nonces),
                || {
                    tickets += 1;
                    Some(InputTicketId::from_raw(tickets))
                },
                block,
            ),
            fixture.viewer.drive(Duration::from_millis(1), |_| {}, block),
        ))
        .await;
        host.unwrap();
        viewer.unwrap();
        if fixture.host.io().unwrap().0.usage().retained_send_records == 0 {
            return;
        }
        assert!(now(host_cx).is_ok());
    }
    panic!("initial native records did not drain within the bounded fixture turns");
}

#[test]
fn local_revoke_emits_the_actual_host_report_and_preserves_uncertain_actions() {
    exercise(false);
}

#[test]
fn a_stopped_controlled_host_reports_revocation_on_its_next_service_turn() {
    exercise(true);
}

fn exercise(service_turn: bool) {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        settle(&mut f, &h).await;
        let expected_lease = f.viewer.input.binding().lease;
        let _ = f.viewer.action(key(true)).unwrap();
        let control = f.host.control();
        control.stop(HostStop::LocalRevoke);
        let driver = f.driver.take().unwrap();
        let ((delivery, revoked), summary) = Box::pin(support::both(
            support::both(
                async {
                    if service_turn {
                        assert_eq!(
                            f.host
                                .drive(
                                    Duration::from_millis(1),
                                    || panic!("revoked input cannot issue challenges"),
                                    || panic!("revoked input cannot issue tickets"),
                                    |_, _| panic!("revoked input cannot dispatch records"),
                                )
                                .await,
                            Err(crate::session_startup::Error::Closed)
                        );
                        f.host
                            .revocation_delivery()
                            .expect("terminal attempt completed")
                    } else {
                        f.host.revoke_and_close(HostStop::LocalRevoke).await
                    }
                },
                async {
                    loop {
                        match f
                            .viewer
                            .drive(
                                Duration::from_millis(1),
                                |_| {
                                    panic!("an unsubmitted action cannot acquire a fabricated receipt");
                                },
                                block,
                            )
                            .await
                        {
                            Err(Error::LeaseRevoked(report)) => break report,
                            other => other.unwrap(),
                        }
                    }
                },
            ),
            driver,
        ))
        .await;
        // The viewer currently closes on receiving the terminal report. Its
        // last transport ACK can be lost; application receipt is asserted below
        // independently, not inferred from sender success or an ACK timeout.
        assert!(matches!(delivery, Ok(()) | Err(quic::Error::Expired)));
        assert_eq!(revoked.lease, expected_lease);
        assert_eq!(revoked.reason, Reason::LocalRevoke);
        assert_eq!(revoked.cleanup, CleanupStage::Fenced);
        assert_eq!(revoked.effects, EffectStage::Unknown);
        assert!(control.is_stopped());
        assert!(summary.handoff_safe());
        assert!(f.effects.lock().unwrap().keys.is_empty());
        assert!(f.viewer.is_closed());
        assert_eq!(f.viewer.pending_actions(), 1);
        assert_eq!(f.viewer.action(key(false)), Err(Error::Closed));
        assert!(f.host.io().is_err());
    });
}

#[test]
fn abandoning_host_terminal_reporting_still_fences_and_releases_native_input() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        let control = f.host.control();
        let report = f.host.revoke_and_close(HostStop::LocalRevoke);
        assert!(control.is_stopped(), "the fence must not wait for polling");
        drop(report);
        assert!(f.host.io().is_err());
        assert!(f.host.revocation_delivery().is_none());
        f.viewer.close();
        let summary = f.driver.take().unwrap().await;
        assert!(summary.handoff_safe());
    });
}
