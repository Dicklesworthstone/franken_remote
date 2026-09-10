//! Actual UDP/TLS and production host/viewer drivers. Identity is an explicit,
//! private fixture; no public constructor admits synthetic or loopback identities.
use super::*;
use crate::session_startup::tests::support as network;
use crate::{
    media::renewal::ObservationRenewal,
    session_startup::{Configuration, Host, Peer},
};
use asupersync::{net::quic_native::StreamRole, types::Budget};
use fr_core::{
    authority::AuthorityPolicy,
    ids::{HostBootId, InputLeaseId, OsSessionId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_wire::negotiation::{ControlBinding, Role};
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

fn config(approval: bool) -> Configuration {
    Configuration {
        offer: Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role: Role::RequestControl,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities: vec![],
        },
        binding: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(13),
        },
        require_approval: approval,
        startup_timeout: Duration::from_secs(2),
        authority: AuthorityPolicy::plan_defaults(),
        transport: Policy {
            critical_send_records: 1,
            ..Policy::default()
        },
    }
}
fn run<F, Fut>(f: F)
where
    F: FnOnce(Cx, Cx) -> Fut,
    Fut: Future<Output = ()>,
{
    let runtime = network::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        asupersync::time::timeout(c.now(), Duration::from_secs(12), f(c, h))
            .await
            .unwrap();
    });
}
async fn pair(c: &Cx, h: &Cx, approval: bool) -> (Host, Viewer) {
    let configuration = config(approval);
    let (client, host) = network::native_pair(c, "localhost", quic::ALPN).await;
    let peer = Peer::Fixture {
        alive: Arc::new(AtomicBool::new(true)),
        until: now(h).unwrap() + 30_000_000,
        control: true,
    };
    let viewer = Viewer::new(
        c.clone(),
        client.unwrap(),
        configuration.offer.clone(),
        configuration.transport,
        Duration::from_secs(2),
    )
    .unwrap();
    let host = Host::start(h.clone(), host.unwrap(), peer, configuration).unwrap();
    (host, viewer)
}
async fn ready(host: &mut Host, viewer: &mut Viewer, approve: bool) {
    let end = Instant::now() + Duration::from_secs(2);
    while !host.is_complete() || !viewer.is_complete() {
        assert!(Instant::now() < end, "actual startup did not complete");
        let (a, b) = Box::pin(network::both(
            host.drive(Duration::from_millis(1)),
            viewer.drive(Duration::from_millis(1)),
        ))
        .await;
        a.unwrap();
        b.unwrap();
        if approve
            && viewer.approval().is_some()
            && let Some(local) = host.approval()
        {
            local.decide(true).unwrap();
        }
    }
}
fn no_other(_: Route, _: &[u8]) -> Result<Disposition, ()> {
    Err(())
}
#[test]
fn native_viewer_negotiates_and_transfers_the_original_connection() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair(&c, &h, false).await;
        let connection = viewer.transport.as_ref().unwrap().binding();
        ready(&mut host, &mut viewer, false).await;
        let mut h = host.finish().unwrap();
        let mut v = viewer.finish().unwrap();
        assert_eq!(v.metadata().binding, h.binding());
        assert_eq!(&v.metadata().selection, h.selection());
        assert!(v.io().unwrap().0.is_bound_to(&connection));
        assert_eq!(v.io().unwrap().0.role(), Ok(StreamRole::Client));
        assert!(h.observation().unwrap().check().is_ok());
        drop(v);
        assert!(c.checkpoint().is_err());
    });
}
#[test]
fn local_approval_is_required_even_with_the_production_viewer() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair(&c, &h, true).await;
        while viewer.approval().is_none() {
            let (a, b) = Box::pin(network::both(
                host.drive(Duration::from_millis(1)),
                viewer.drive(Duration::from_millis(1)),
            ))
            .await;
            a.unwrap();
            b.unwrap();
        }
        assert!(!host.is_complete());
        assert!(!viewer.is_complete());
        assert!(host.observation_until.is_none());
        ready(&mut host, &mut viewer, true).await;
        let v = viewer.finish().unwrap();
        assert_eq!(
            v.metadata().binding.remote_session,
            config(true).binding.remote_session
        );
    });
}
#[test]
fn production_viewer_responses_keep_observation_alive_beyond_the_original_grant() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair(&c, &h, false).await;
        ready(&mut host, &mut viewer, false).await;
        let original_until = host.observation_until.unwrap();
        let mut host = host.finish().unwrap();
        let control = host.observation().unwrap();
        let limits = host.selection().limits;
        let (transport, routes) = host.io().unwrap();
        let mut renewal =
            ObservationRenewal::new(control.clone(), transport, routes, limits).unwrap();
        let mut viewer = viewer.finish().unwrap();
        let mut nonce = 0;
        while now(&h).unwrap() < original_until + 100_000 {
            let (transport, _) = host.io().unwrap();
            renewal.receive(transport, no_other).unwrap();
            renewal
                .service(transport, || {
                    nonce += 1;
                    Ok(nonce)
                })
                .unwrap();
            let (a, b) = Box::pin(network::both(
                renewal.drive(transport, Duration::from_millis(10)),
                viewer.drive(Duration::from_millis(10), no_other),
            ))
            .await;
            a.unwrap();
            b.unwrap();
        }
        assert!(renewal.renewed_until().unwrap().as_micros() > original_until);
        assert!(control.check().is_ok());
        assert!(viewer.check().is_ok());
    });
}
#[test]
fn response_backpressure_keeps_the_original_local_deadline_and_exact_bytes() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair(&c, &h, false).await;
        ready(&mut host, &mut viewer, false).await;
        let mut viewer = viewer.finish().unwrap();
        // Fill the actual one-record critical queue with an unrelated bounded
        // control record. No simulation of transport admission is used.
        let binding = viewer.metadata().binding;
        let mut blocker = [0; fr_wire::clock::PROBE_BYTES];
        fr_wire::clock::encode(
            fr_wire::clock::Message::Probe { sequence: 1 },
            binding,
            &ProtocolLimits::ABSOLUTE,
            &mut blocker,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        // Drain prior binding-ack retention before occupying the queue.
        loop {
            let (transport, routes) = viewer.io().unwrap();
            match transport.send(
                &c,
                Route::Stream(routes.outbound),
                &blocker,
                now(&c).unwrap() + 1_000_000,
                || true,
            ) {
                Ok(()) => break,
                Err(quic::Error::Backpressure) => {
                    let (a, b) = Box::pin(network::both(
                        host.drive(Duration::from_millis(1)),
                        viewer
                            .transport
                            .drive(&c, Duration::from_millis(1), || true),
                    ))
                    .await;
                    a.unwrap();
                    b.unwrap();
                }
                Err(e) => panic!("{e:?}"),
            }
        }
        // Feed an exactly validated challenge to the same production responder;
        // this case isolates retention at real QUIC send admission.
        let mut challenge = [0; authority::OBSERVATION_CHALLENGE_BYTES];
        authority::encode(
            Message::Challenge {
                scope: Scope::Observation,
                nonce: 44,
                deadline_micros: 55,
            },
            authority::Binding {
                channel: binding.id,
                session: binding.remote_session,
            },
            &ProtocolLimits::ABSOLUTE,
            &mut challenge,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        viewer
            .responder
            .accept(&challenge, ClientInstant(now(&c).unwrap()))
            .unwrap();
        let deadline = viewer.responder.response_deadline().unwrap();
        let bytes = viewer
            .responder
            .pending(ClientInstant(now(&c).unwrap()))
            .unwrap()
            .unwrap()
            .to_vec();
        for _ in 0..4 {
            viewer.send_response().unwrap();
        }
        assert_eq!(viewer.responder.response_deadline(), Some(deadline));
        assert_eq!(
            viewer
                .responder
                .pending(ClientInstant(now(&c).unwrap()))
                .unwrap()
                .unwrap(),
            bytes
        );
        assert_eq!(viewer.transport.usage().critical_send_records, 1);
    });
}
#[test]
fn dropping_unpolled_startup_or_session_drive_closes_that_viewer() {
    run(|c, h| async move {
        let (_, mut viewer) = pair(&c, &h, false).await;
        drop(viewer.drive(Duration::from_millis(1)));
        assert!(viewer.tick().is_err());
        assert!(viewer.transport.as_ref().unwrap().is_closed());
    });
    run(|c, h| async move {
        let (mut host, mut viewer) = pair(&c, &h, false).await;
        ready(&mut host, &mut viewer, false).await;
        let mut viewer = viewer.finish().unwrap();
        drop(viewer.drive(Duration::from_millis(1), no_other));
        assert!(viewer.is_closed());
        assert!(viewer.check().is_err());
    });
}
#[test]
fn silent_host_expires_the_viewer_without_inventing_a_host_clock_correlation() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair(&c, &h, false).await;
        ready(&mut host, &mut viewer, false).await;
        let mut viewer = viewer.finish().unwrap();
        let end = viewer.heard_until;
        std::thread::sleep(Duration::from_micros(end - now(&c).unwrap() + 1_000));
        assert_eq!(viewer.check(), Err(Error::Expired));
        assert!(viewer.is_closed());
    });
}
#[test]
fn control_challenges_are_dispatched_without_observation_renewal_side_effects() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair(&c, &h, false).await;
        ready(&mut host, &mut viewer, false).await;
        let mut host = host.finish().unwrap();
        let mut viewer = viewer.finish().unwrap();
        let until = viewer.heard_until;
        let binding = host.binding();
        let mut bytes = [0; authority::MAX_AUTHORITY_BYTES];
        let n = authority::encode(
            Message::Challenge {
                scope: Scope::Control(InputLeaseId::from_raw(21)),
                nonce: 5,
                deadline_micros: 6,
            },
            authority::Binding {
                channel: binding.id,
                session: binding.remote_session,
            },
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        let (transport, routes) = host.io().unwrap();
        let end = now(&h).unwrap() + 1_000_000;
        loop {
            match transport.send(&h, Route::Stream(routes.outbound), &bytes[..n], end, || {
                true
            }) {
                Ok(()) => break,
                Err(quic::Error::Backpressure) => {
                    let (a, b) = Box::pin(network::both(
                        transport.drive(&h, Duration::from_millis(1), || true),
                        viewer.drive(Duration::from_millis(1), no_other),
                    ))
                    .await;
                    a.unwrap();
                    b.unwrap();
                }
                Err(e) => panic!("{e:?}"),
            }
        }
        let mut seen = 0;
        while seen == 0 {
            assert!(now(&h).unwrap() < end);
            let (a, b) = Box::pin(network::both(
                transport.drive(&h, Duration::from_millis(1), || true),
                viewer.drive(Duration::from_millis(1), |_, b| {
                    assert_eq!(b, &bytes[..n]);
                    seen += 1;
                    Ok(Disposition::Consumed)
                }),
            ))
            .await;
            a.unwrap();
            b.unwrap();
        }
        assert_eq!(seen, 1);
        assert_eq!(viewer.heard_until, until);
        assert!(viewer.responder.response_deadline().is_none());
    });
}
