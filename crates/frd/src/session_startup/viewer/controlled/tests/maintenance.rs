//! Real authenticated UDP/TLS and the production viewer loop. The peer's
//! messages, codec completion and native sink are explicit fixtures; these
//! tests do not claim a live compositor or host-side challenge issuance.
use super::*;
use fr_wire::{
    authority::{self, Binding, Message, Scope},
    input::{InputDelivery, InputDirection},
    lease_revoked::{self, CleanupStage, EffectStage, Reason, Revoked},
};

fn binding(state: &Fixture) -> Binding {
    Binding {
        channel: state.viewer.session.opened.binding.id,
        session: state.viewer.session.opened.binding.remote_session,
    }
}
async fn cleanup(state: &mut Fixture) {
    state.viewer.close();
    state.host.close();
    assert!(state.driver.take().unwrap().await.handoff_safe());
}
// Only original bytes/deadlines are retried under bounded queue pressure. The
// host's application handler is intentionally not used for synthetic messages.
async fn admit(state: &mut Fixture, h: &Cx, bytes: &[u8], until: u64) {
    loop {
        assert!(now(h).unwrap() < until, "fixture admission expired");
        let (q, routes) = state.host.io().unwrap();
        match q.send(h, Route::Stream(routes.outbound), bytes, until, || true) {
            Ok(()) => return,
            Err(quic::Error::Backpressure) => {}
            other => panic!("fixture admission: {other:?}"),
        }
        let (host, viewer) = Box::pin(support::both(
            q.drive(h, Duration::from_millis(1), || true),
            state.viewer.drive(Duration::from_millis(1), |_| {}, block),
        ))
        .await;
        host.unwrap();
        viewer.unwrap();
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn control_challenge_and_response_cross_a_visibility_gap_without_sending_input() {
    run(|c, h| async move {
        let mut state = Box::pin(fixture(&c, &h)).await;
        let scope = Scope::Control(state.viewer.input.binding().lease);
        let context = binding(&state);
        let until = now(&h).unwrap() + 200_000;
        let mut bytes = [0; authority::MAX_AUTHORITY_BYTES];
        let n = authority::encode(
            Message::Challenge {
                scope,
                nonce: 91_117,
                deadline_micros: now(&h).unwrap() + 1_000_000,
            },
            context,
            &state.viewer.input.protocol_limits(),
            &mut bytes,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        admit(&mut state, &h, &bytes[..n], until).await;
        let original = state.viewer.action(key(true)).unwrap();
        let pending = state.viewer.pending.as_ref().unwrap();
        let exact = pending.bytes[..pending.len].to_vec();
        let deadline = pending.until;
        submit_successor(&mut state, &c);
        let limits = state.viewer.input.protocol_limits();
        let mut responses = 0;
        while now(&h).unwrap() < until && responses == 0 {
            let (q, routes) = state.host.io().unwrap();
            let (host, viewer) = Box::pin(support::both(
                q.drive(&h, Duration::from_millis(1), || true),
                state.viewer.drive(Duration::from_millis(1), |_| {}, block),
            ))
            .await;
            host.unwrap();
            viewer.unwrap();
            if let Some(clock) = &mut state.host_clock {
                clock.receive(q, block).unwrap();
                clock.service(q).unwrap();
            }
            q.receive_ready(
                &h,
                || true,
                |route| route == Route::Stream(routes.inbound),
                |_, record| {
                    if record.get(6..8) != Some(&(Kind::ChallengeResponse as u16).to_be_bytes()) {
                        return Ok(Disposition::Blocked);
                    }
                    match authority::decode(
                        record,
                        context,
                        &limits,
                        InputDirection::ViewerToHost,
                        InputDelivery::Reliable,
                    )
                    .unwrap()
                    {
                        Message::Response {
                            scope: actual,
                            nonce: 91_117,
                        } => {
                            assert_eq!(actual, scope);
                            responses += 1;
                        }
                        // A startup observation response can still be queued.
                        // This raw peer does not apply it as authority evidence.
                        Message::Response {
                            scope: Scope::Observation,
                            ..
                        } => {}
                        other => panic!("unexpected response: {other:?}"),
                    }
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        }
        assert_eq!(
            responses, 1,
            "renewal was starved behind compositor visibility"
        );
        loop {
            assert!(
                now(&c).unwrap() < deadline,
                "input admission remained blocked"
            );
            let refusal = state.viewer.channels.send_prepared(
                &c,
                &mut state.viewer.session.transport,
                &exact,
                deadline,
                || true,
                || Err::<(), ()>(()),
            );
            match refusal {
                Ok(quic::SendAdmission::Refused(())) => {
                    // The transport has room. The real input preparation gate,
                    // not queue pressure, must still keep these bytes unsent.
                    state.viewer.send().unwrap();
                    break;
                }
                Err(crate::input_quic::Error::Transport(quic::Error::Backpressure)) => {}
                other => panic!("unexpected prepared admission: {other:?}"),
            }
            let (host, viewer) = Box::pin(support::both(
                state
                    .host
                    .io()
                    .unwrap()
                    .0
                    .drive(&h, Duration::from_millis(1), || true),
                state.viewer.drive(Duration::from_millis(1), |_| {}, block),
            ))
            .await;
            host.unwrap();
            viewer.unwrap();
        }
        assert_eq!(original.sequence, 0);
        assert_eq!(state.viewer.input.binding().lease, creds().lease);
        assert!(state.viewer.input.control_response_deadline().is_none());
        assert_eq!(state.viewer.pending_actions(), 1);
        let pending = state.viewer.pending.as_ref().unwrap();
        assert_eq!(pending.until, deadline);
        assert_eq!(&pending.bytes[..pending.len], exact);
        assert_eq!(state.effects.lock().unwrap().keys, [] as [bool; 0]);
        assert!(matches!(
            state
                .viewer
                .input
                .view_deadline(ClientInstant(now(&c).unwrap())),
            Err(presentation::Error::Input(
                fr_client::input::Error::NoPresentedView
            ))
        ));
        cleanup(&mut state).await;
    });
}

#[test]
fn an_authenticated_revocation_is_dispatched_before_the_visibility_callback() {
    run(|c, h| async move {
        let mut state = Box::pin(fixture(&c, &h)).await;
        let report = Revoked {
            lease: state.viewer.input.binding().lease,
            reason: Reason::LocalRevoke,
            cleanup: CleanupStage::Fenced,
            effects: EffectStage::Unknown,
        };
        let mut bytes = [0; lease_revoked::REVOKED_BYTES];
        lease_revoked::encode(
            report,
            binding(&state),
            &state.viewer.input.protocol_limits(),
            &mut bytes,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        let until = now(&h).unwrap() + 200_000;
        admit(&mut state, &h, &bytes, until).await;
        let _ = state.viewer.action(key(true)).unwrap();
        submit_successor(&mut state, &c);
        let mut received = false;
        let mut results = 0;
        while now(&h).unwrap() < until {
            let (q, _) = state.host.io().unwrap();
            let (host, viewer) = Box::pin(support::both(
                q.drive(&h, Duration::from_millis(1), || true),
                state
                    .viewer
                    .drive(Duration::from_millis(1), |_| results += 1, block),
            ))
            .await;
            if let Err(Error::LeaseRevoked(actual)) = viewer {
                assert_eq!(actual, report);
                received = true;
                break;
            }
            host.unwrap();
            viewer.unwrap();
        }
        assert!(
            received,
            "revocation was starved behind compositor visibility"
        );
        assert!(state.viewer.is_closed());
        assert!(!state.viewer.pending_send());
        assert_eq!(
            state.viewer.pending_actions(),
            1,
            "no invented input receipt"
        );
        assert_eq!(results, 0);
        assert_eq!(state.effects.lock().unwrap().keys, [] as [bool; 0]);
        assert!(state.viewer.visible(1).is_err());
        cleanup(&mut state).await;
    });
}

#[test]
fn network_maintenance_does_not_encode_captured_keys_from_an_unconfirmed_view() {
    run(|c, h| async move {
        let mut state = Box::pin(fixture(&c, &h)).await;
        let mut source = state.viewer.capture_input().unwrap();
        source
            .push(
                super::super::events::Event::Key {
                    key: PhysicalKey::new(4).unwrap(),
                    transition: KeyTransition::Press,
                },
                source.clock().unwrap(),
            )
            .unwrap();
        submit_successor(&mut state, &c);
        state
            .viewer
            .drive(Duration::ZERO, |_| {}, block)
            .await
            .unwrap();
        assert!(!state.viewer.is_closed());
        assert!(!state.viewer.pending_send());
        assert_eq!(state.viewer.pending_actions(), 0);
        assert_eq!(state.effects.lock().unwrap().keys, [] as [bool; 0]);
        // Only an actual callback for the exact decoded frame opens admission.
        state.viewer.visible(1).unwrap();
        state
            .viewer
            .drive(Duration::ZERO, |_| {}, block)
            .await
            .unwrap();
        assert_eq!(state.viewer.pending_actions(), 1);
        assert!(state.viewer.input.holds_key(PhysicalKey::new(4).unwrap()));
        cleanup(&mut state).await;
        assert!(source.control().is_stopped());
    });
}

#[path = "maintenance/clipboard.rs"]
mod clipboard;
