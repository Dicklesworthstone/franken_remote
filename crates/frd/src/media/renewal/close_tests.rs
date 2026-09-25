//! Real local TLS/UDP with private fixture admission, not a live-tailnet claim.
use super::*;
use crate::session_startup::{
    Error as StartupError, HostSession, Viewer, ViewerSession, connection_test_host,
    test_network as network,
};
use asupersync::{cx::Cx, types::Budget};
use fr_wire::{
    closure::{self, CloseRequest, Reason},
    negotiation::{Offer, Role},
};

async fn pair(c: &Cx, h: &Cx) -> (HostSession, ViewerSession) {
    let (client, server) = network::native_pair(c, "localhost", fr_transport::quic::ALPN).await;
    let offer = Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: vec![],
    };
    let policy = fr_transport::quic::Policy::default();
    let mut host = connection_test_host(h.clone(), server.unwrap(), offer.clone(), policy);
    let mut viewer = Viewer::new(
        c.clone(),
        client.unwrap(),
        offer,
        policy,
        Duration::from_secs(2),
    )
    .unwrap();
    for _ in 0..1000 {
        let (a, b) = Box::pin(network::both(
            host.drive(Duration::from_millis(1)),
            viewer.drive(Duration::from_millis(1)),
        ))
        .await;
        a.unwrap();
        b.unwrap();
        if let Some(approval) = host.approval() {
            approval.decide(true).unwrap();
        }
        if host.is_complete() && viewer.is_complete() {
            return (
                host.finish().unwrap().into_running().unwrap(),
                viewer.finish().unwrap(),
            );
        }
    }
    panic!("startup did not complete");
}
fn request(host: &HostSession) -> [u8; closure::REQUEST_BYTES] {
    let parent = host.binding();
    let mut bytes = [0; closure::REQUEST_BYTES];
    closure::encode_request(
        CloseRequest {
            reason: Reason::Requested,
        },
        Binding {
            channel: parent.id,
            session: parent.remote_session,
        },
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes
}
async fn terminal(host: &mut HostSession, viewer: &mut ViewerSession) -> (StartupError, usize) {
    let mut callbacks = 0;
    let mut nonce = 0_u128;
    for _ in 0..500 {
        let (result, _) = Box::pin(network::both(
            host.drive(
                Duration::from_millis(1),
                || {
                    nonce += 1;
                    Ok(nonce)
                },
                |_, _| {
                    callbacks += 1;
                    Ok(Disposition::Consumed)
                },
            ),
            viewer.drive(Duration::from_millis(1), |_, _| Ok(Disposition::Consumed)),
        ))
        .await;
        if let Err(error) = result {
            return (error, callbacks);
        }
    }
    panic!("close request did not terminate the host session");
}

#[test]
fn close_and_invalid_close_fence_before_following_application_records() {
    for case in 0..3 {
        let runtime = network::runtime();
        let c = runtime.request_cx_with_budget(Budget::INFINITE);
        let h = runtime.request_cx_with_budget(Budget::INFINITE);
        runtime.block_on(async {
            let (mut host, mut viewer) = pair(&c, &h).await;
            let observation = host.observation().unwrap();
            let mut bytes = request(&host);
            let expected = match case {
                0 => Error::PeerClosed,
                1 => {
                    bytes[39] ^= 1;
                    Error::Wire(WireError::InvalidBinding)
                }
                _ => {
                    bytes[40] = 0;
                    Error::Wire(WireError::UnsupportedKind)
                }
            };
            let deadline = c.timer_driver().unwrap().now().as_nanos() / 1000 + 1_000_000;
            let (q, routes) = viewer.io().unwrap();
            q.send(&c, Route::Stream(routes.outbound), &bytes, deadline, || {
                true
            })
            .unwrap();
            // A following control-family record must not reach the application,
            // even if both arrive in one turn. No input effect is simulated here.
            bytes[6..8].copy_from_slice(&(Kind::ControlRequest as u16).to_be_bytes());
            q.send(&c, Route::Stream(routes.outbound), &bytes, deadline, || {
                true
            })
            .unwrap();
            let (error, callbacks) = terminal(&mut host, &mut viewer).await;
            assert_eq!(error, StartupError::Renewal(expected));
            assert_eq!(callbacks, 0);
            assert!(observation.check().is_err());
            assert!(observation.renew(1).is_err(), "close cannot be renewed");
            assert!(host.io().is_err());
        });
    }
}

#[test]
fn closing_one_session_does_not_revoke_another_owner_with_equal_numeric_ids() {
    let runtime = network::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    let c2 = runtime.request_cx_with_budget(Budget::INFINITE);
    let h2 = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut host, mut viewer) = pair(&c, &h).await;
        let (mut sibling, _sibling_viewer) = pair(&c2, &h2).await;
        assert_eq!(host.binding(), sibling.binding()); // deliberate numeric reuse
        let unaffected = sibling.observation().unwrap();
        let bytes = request(&host);
        let deadline = c.timer_driver().unwrap().now().as_nanos() / 1000 + 1_000_000;
        let (q, routes) = viewer.io().unwrap();
        q.send(&c, Route::Stream(routes.outbound), &bytes, deadline, || {
            true
        })
        .unwrap();
        let (error, callbacks) = terminal(&mut host, &mut viewer).await;
        assert_eq!(error, StartupError::Renewal(Error::PeerClosed));
        assert_eq!(callbacks, 0);
        assert!(unaffected.check().is_ok());
        assert!(sibling.check().is_ok());
    });
}
