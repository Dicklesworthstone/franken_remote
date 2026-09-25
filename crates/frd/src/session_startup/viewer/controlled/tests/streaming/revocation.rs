//! The network test uses real authenticated QUIC and the real viewer loop.
//! The host report, codec output, and native sink are explicit test fixtures;
//! these tests do not qualify host-side notification delivery or X11 effects.
use super::*;
use fr_wire::{
    authority::Binding,
    input::{InputDelivery, InputDirection},
    lease_revoked::{self, CleanupStage, EffectStage, REVOKED_BYTES, Reason, Revoked},
};

fn binding(viewer: &ControlledViewer) -> Binding {
    Binding {
        channel: viewer.session.opened.binding.id,
        session: viewer.session.opened.binding.remote_session,
    }
}
fn notice(viewer: &ControlledViewer) -> (Revoked, [u8; REVOKED_BYTES]) {
    let report = Revoked {
        lease: viewer.input.binding().lease,
        reason: Reason::LocalRevoke,
        cleanup: CleanupStage::Fenced,
        effects: EffectStage::Unknown,
    };
    let mut bytes = [0; REVOKED_BYTES];
    assert_eq!(
        lease_revoked::encode(
            report,
            binding(viewer),
            &viewer.input.protocol_limits(),
            &mut bytes,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        ),
        Ok(REVOKED_BYTES)
    );
    (report, bytes)
}
async fn cleanup(fixture: &mut Fixture) {
    fixture.observation.revoke();
    fixture.viewer.close();
    let summary = fixture.driver.take().unwrap().await;
    assert!(summary.handoff_safe());
}

#[test]
fn revoked_record_stops_the_real_viewer_without_fabricating_action_receipts() {
    run(|c, h| async move {
        let mut fixture = Box::pin(fixture(&c, &h)).await;
        let (report, bytes) = notice(&fixture.viewer);
        let route = Route::Stream(fixture.host.io().unwrap().1.outbound);
        let until = now(&h).unwrap() + 200_000;
        admit_record(
            &mut fixture.host,
            &mut fixture.viewer,
            &h,
            route,
            &bytes,
            until,
        )
        .await;
        let _ = fixture.viewer.action(key(true)).unwrap();
        assert_eq!(fixture.viewer.pending_actions(), 1);
        let mut received = false;
        let mut receipts = 0;
        while now(&h).unwrap() < until {
            // Deliver the prepared report through the original actual UDP/TLS
            // connection. Do not run the host action handler for this fixture.
            let (host_result, viewer_result) = Box::pin(support::both(
                fixture
                    .host
                    .io()
                    .unwrap()
                    .0
                    .drive(&h, Duration::from_millis(1), || {
                        now(&h).is_ok_and(|at| at < until)
                    }),
                fixture
                    .viewer
                    .drive(Duration::from_millis(1), |_| receipts += 1, block),
            ))
            .await;
            if let Err(Error::LeaseRevoked(actual)) = viewer_result {
                assert_eq!(actual, report);
                received = true;
                break;
            }
            viewer_result.unwrap();
            host_result.unwrap();
        }
        assert!(received, "the control report was not dispatched");
        assert!(fixture.viewer.is_closed());
        assert!(!fixture.viewer.pending_send());
        assert_eq!(receipts, 0);
        assert_eq!(fixture.viewer.pending_actions(), 1);
        assert_eq!(fixture.viewer.action(key(false)), Err(Error::Closed));
        assert!(fixture.viewer.input.control_response_deadline().is_none());
        cleanup(&mut fixture).await;
    });
}

#[test]
fn a_stale_lease_or_wrong_control_binding_cannot_revoke_the_current_owner() {
    run(|c, h| async move {
        let mut fixture = Box::pin(fixture(&c, &h)).await;
        let (_, mut bytes) = notice(&fixture.viewer);
        let binding = binding(&fixture.viewer);
        bytes[55] ^= 1; // Last byte of the full lease ID, not the compact route.
        assert_eq!(
            fixture.viewer.input.accept_lease_revoked(&bytes, binding),
            Err(presentation::Error::Input(fr_client::input::Error::Wire(
                WireError::InvalidBinding,
            )))
        );
        assert!(fixture.viewer.input.stopped().is_none());
        bytes[55] ^= 1;
        let wrong = Binding {
            channel: binding.channel.checked_add(1).unwrap(),
            ..binding
        };
        assert!(
            fixture
                .viewer
                .input
                .accept_lease_revoked(&bytes, wrong)
                .is_err()
        );
        assert!(fixture.viewer.input.stopped().is_none());
        cleanup(&mut fixture).await;
    });
}

#[test]
fn revocation_discards_a_pending_renewal_and_cannot_be_reversed_by_a_ticket() {
    run(|c, h| async move {
        let mut fixture = Box::pin(fixture(&c, &h)).await;
        let (report, bytes) = notice(&fixture.viewer);
        let binding = binding(&fixture.viewer);
        let at = ClientInstant(now(&c).unwrap());
        let mut challenge = [0; fr_wire::authority::MAX_AUTHORITY_BYTES];
        let n = fr_wire::authority::encode(
            fr_wire::authority::Message::Challenge {
                scope: fr_wire::authority::Scope::Control(report.lease),
                nonce: 9001,
                deadline_micros: now(&h).unwrap() + 1_000_000,
            },
            binding,
            &fixture.viewer.input.protocol_limits(),
            &mut challenge,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        fixture
            .viewer
            .input
            .accept_control_challenge(&challenge[..n], at)
            .unwrap();
        assert!(fixture.viewer.input.control_response_deadline().is_some());
        assert_eq!(
            fixture.viewer.input.accept_lease_revoked(&bytes, binding),
            Ok(report)
        );
        assert!(fixture.viewer.input.control_response_deadline().is_none());
        assert!(
            fixture
                .viewer
                .input
                .ticket(InputTicketId::from_raw(99), at)
                .is_err()
        );
        assert!(fixture.viewer.input.pending_control_response(at).is_err());
        cleanup(&mut fixture).await;
    });
}

#[test]
fn terminal_report_remains_readable_after_local_view_failure() {
    run(|c, h| async move {
        let mut fixture = Box::pin(fixture(&c, &h)).await;
        let (report, bytes) = notice(&fixture.viewer);
        let binding = binding(&fixture.viewer);
        fixture.viewer.input.hidden();
        assert_eq!(
            fixture.viewer.input.accept_lease_revoked(&bytes, binding),
            Ok(report)
        );
        assert_eq!(fixture.viewer.input.stopped(), Some(StopReason::FocusLost));
        cleanup(&mut fixture).await;
    });
}
