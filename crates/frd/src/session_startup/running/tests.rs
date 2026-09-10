//! Production persistent drivers over actual localhost TLS/UDP. Identity is a
//! private fixture. Slow refresh futures test scheduling, not live Tailscale.
use super::*;
use crate::session_startup::{Configuration, Host, Peer, Viewer, ViewerSession, tests::support};
use asupersync::types::Budget;
use fr_core::{authority::AuthorityPolicy, ids::*, limits::ProtocolLimits};
use fr_transport::quic::{self, Policy};
use fr_wire::{
    authority::{self, Binding, Message, Scope},
    input::{InputDelivery as T, InputDirection as D},
    negotiation::{Offer, Role},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
fn run<F, Fut>(f: F)
where
    F: FnOnce(Cx, Cx) -> Fut,
    Fut: Future<Output = ()>,
{
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        asupersync::time::timeout(c.now(), Duration::from_secs(12), f(c, h))
            .await
            .unwrap();
    });
}
async fn pair(c: &Cx, h: &Cx) -> (HostSession, ViewerSession) {
    pair_with_capabilities(c, h, vec![]).await
}
async fn pair_with_capabilities(
    c: &Cx,
    h: &Cx,
    capabilities: Vec<fr_wire::negotiation::Capability>,
) -> (HostSession, ViewerSession) {
    let cfg = Configuration {
        offer: Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role: Role::RequestControl,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities,
        },
        binding: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(13),
        },
        require_approval: false,
        startup_timeout: Duration::from_secs(2),
        authority: AuthorityPolicy::plan_defaults(),
        transport: Policy {
            critical_send_records: 1,
            ..Policy::default()
        },
    };
    let (client, host) = support::native_pair(c, "localhost", quic::ALPN).await;
    let peer = Peer::Fixture {
        alive: Arc::new(AtomicBool::new(true)),
        until: now(h).unwrap() + 30_000_000,
        control: true,
    };
    let mut viewer = Viewer::new(
        c.clone(),
        client.unwrap(),
        cfg.offer.clone(),
        cfg.transport,
        Duration::from_secs(2),
    )
    .unwrap();
    let mut h = Host::start(h.clone(), host.unwrap(), peer, cfg).unwrap();
    while !viewer.is_complete() || !h.is_complete() {
        let (a, b) = Box::pin(support::both(
            h.drive(Duration::from_millis(1)),
            viewer.drive(Duration::from_millis(1)),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
    (
        h.finish().unwrap().into_running().unwrap(),
        viewer.finish().unwrap(),
    )
}
fn no_other(_: Route, _: &[u8]) -> Result<Disposition, ()> {
    Err(())
}
fn nonce(n: &mut u128) -> Result<u128, ()> {
    *n = n.checked_add(1).ok_or(())?;
    Ok(*n)
}
fn control_reply(session: RemoteSessionId, channel: u32) -> Vec<u8> {
    let mut bytes = [0; 128];
    let n = authority::encode(
        Message::Response {
            scope: Scope::Control(InputLeaseId::from_raw(19)),
            nonce: 77,
        },
        Binding { session, channel },
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        D::ViewerToHost,
        T::Reliable,
    )
    .unwrap();
    bytes[..n].to_vec()
}
async fn queue_reply(c: &Cx, host: &mut HostSession, viewer: &mut ViewerSession) -> Vec<u8> {
    let (_, routes) = viewer.io().unwrap();
    let bytes = control_reply(host.binding().remote_session, routes.outbound.binding);
    let deadline = now(c).unwrap() + 1_000_000;
    loop {
        assert!(now(c).unwrap() < deadline, "control record never admitted");
        match viewer.io().unwrap().0.send(
            c,
            Route::Stream(routes.outbound),
            &bytes,
            deadline,
            || true,
        ) {
            Ok(()) => return bytes,
            Err(quic::Error::Backpressure) => {
                // Startup's BindingAccepted can still occupy the sole critical
                // credit until its real ACK arrives. Preserve these reply bytes.
                let h = host.opened.cx.clone();
                let (a, b) = Box::pin(support::both(
                    host.opened
                        .transport
                        .drive(&h, Duration::from_millis(2), || true),
                    viewer
                        .io()
                        .unwrap()
                        .0
                        .drive(c, Duration::from_millis(2), || true),
                ))
                .await;
                a.unwrap();
                b.unwrap();
            }
            Err(e) => panic!("reply send failed: {e:?}"),
        }
    }
}

#[test]
fn both_persistent_drivers_keep_the_original_observation_alive_across_multiple_cadences() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair(&c, &h).await;
        let binding = host.binding();
        let control = host.observation().unwrap();
        let initial = control
            .deadline(Duration::from_secs(3))
            .unwrap()
            .time()
            .as_nanos()
            / 1000;
        let conn = host.io().unwrap().0.binding();
        let mut n = 0;
        while now(&h).unwrap() < initial + 150_000 {
            let (a, b) = Box::pin(support::both(
                host.drive(Duration::from_millis(10), || nonce(&mut n), no_other),
                viewer.drive(Duration::from_millis(10), no_other),
            ))
            .await;
            a.unwrap();
            b.unwrap();
        }
        assert!(n >= 3);
        assert!(host.renewed_until().unwrap().as_micros() > initial);
        assert_eq!(host.binding(), binding);
        assert!(host.io().unwrap().0.is_bound_to(&conn));
        assert!(control.check().is_ok());
        assert!(viewer.check().is_ok());
        host.close();
        assert!(control.check().is_err());
    });
}
#[test]
fn other_control_records_are_dispatched_without_becoming_observation_responses() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair(&c, &h).await;
        let bytes = queue_reply(&c, &mut host, &mut viewer).await;
        let mut seen = 0;
        let mut n = 0;
        let deadline = Instant::now() + Duration::from_secs(1);
        while seen == 0 || host.renewed_until().is_none() {
            assert!(Instant::now() < deadline);
            let (a, b) = Box::pin(support::both(
                host.drive(
                    Duration::from_millis(2),
                    || nonce(&mut n),
                    |_, record| {
                        assert_eq!(record, bytes);
                        seen += 1;
                        Ok(Disposition::Consumed)
                    },
                ),
                viewer.drive(Duration::from_millis(2), no_other),
            ))
            .await;
            a.unwrap();
            b.unwrap();
        }
        assert_eq!(seen, 1);
        assert!(host.check().is_ok());
    });
}
#[test]
fn delayed_refresh_keeps_real_udp_and_application_dispatch_running() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair(&c, &h).await;
        let control = host.observation().unwrap();
        let conn = host.io().unwrap().0.binding();
        let bytes = queue_reply(&c, &mut host, &mut viewer).await;
        let done = Arc::new(AtomicBool::new(false));
        let completed = done.clone();
        let refresh = async {
            asupersync::time::sleep(h.now(), Duration::from_millis(80)).await;
            completed.store(true, Ordering::Release);
            Ok(())
        };
        let mut n = 0;
        let mut seen = 0;
        let step = RefreshTurn {
            cx: &h,
            control: &control,
            until: now(&h).unwrap() + 500_000,
            wait: Duration::from_millis(4),
        };
        let result = Box::pin(support::both(
            pump_refresh(
                &mut host.renewal,
                &mut host.opened.transport,
                refresh,
                step,
                &mut || nonce(&mut n),
                &mut |_, record| {
                    assert!(
                        !done.load(Ordering::Acquire),
                        "network blocked behind the refresh"
                    );
                    assert_eq!(record, bytes);
                    seen += 1;
                    Ok(Disposition::Consumed)
                },
            ),
            async {
                while !done.load(Ordering::Acquire) {
                    viewer
                        .drive(Duration::from_millis(4), no_other)
                        .await
                        .unwrap();
                }
            },
        ))
        .await;
        result.0.unwrap();
        assert_eq!(seen, 1);
        assert!(host.renewed_until().is_some());
        assert!(host.io().unwrap().0.is_bound_to(&conn));
        assert!(control.check().is_ok());
        assert!(viewer.check().is_ok());
    });
}
#[test]
fn immediately_ready_refresh_does_not_cancel_its_inflight_quic_turn() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair(&c, &h).await;
        let control = host.observation().unwrap();
        let mut n = 0;
        let turn = RefreshTurn {
            cx: &h,
            control: &control,
            until: now(&h).unwrap() + 500_000,
            wait: Duration::from_millis(5),
        };
        let (a, b) = Box::pin(support::both(
            pump_refresh(
                &mut host.renewal,
                &mut host.opened.transport,
                async { Ok(()) },
                turn,
                &mut || nonce(&mut n),
                &mut no_other,
            ),
            viewer.drive(Duration::from_millis(5), no_other),
        ))
        .await;
        a.unwrap();
        b.unwrap();
        assert!(control.check().is_ok());
        assert!(!host.io().unwrap().0.is_closed());
        for _ in 0..5 {
            let (a, b) = Box::pin(support::both(
                host.drive(Duration::from_millis(2), || nonce(&mut n), no_other),
                viewer.drive(Duration::from_millis(2), no_other),
            ))
            .await;
            a.unwrap();
            b.unwrap();
        }
        assert!(host.renewed_until().is_some());
    });
}
#[test]
fn dropped_unpolled_drive_nonce_failure_and_dispatch_failure_revoke_escaped_observation() {
    for mode in 0..3 {
        run(|c, h| async move {
            let (mut host, mut viewer) = pair(&c, &h).await;
            let control = host.observation().unwrap();
            match mode {
                0 => drop(host.drive(Duration::from_millis(1), || Ok(1), no_other)),
                1 => assert!(matches!(
                    host.drive(Duration::from_millis(1), || Err(()), no_other)
                        .await,
                    Err(Error::Renewal(_))
                )),
                _ => {
                    let _bytes = queue_reply(&c, &mut host, &mut viewer).await;
                    let mut n = 0;
                    let deadline = Instant::now() + Duration::from_secs(1);
                    loop {
                        assert!(Instant::now() < deadline);
                        let (a, _) = Box::pin(support::both(
                            host.drive(Duration::from_millis(2), || nonce(&mut n), no_other),
                            viewer.drive(Duration::from_millis(2), no_other),
                        ))
                        .await;
                        if a.is_err() {
                            break;
                        }
                    }
                }
            }
            assert!(control.check().is_err());
            assert!(host.check().is_err());
        });
    }
}
#[test]
fn peer_permission_revocation_is_not_hidden_by_a_quiet_healthy_connection() {
    run(|c, h| async move {
        let (mut host, _viewer) = pair(&c, &h).await;
        let control = host.observation().unwrap();
        host.opened.peer.revoke();
        assert!(
            host.drive(Duration::from_millis(1), || Ok(1), no_other)
                .await
                .is_err()
        );
        assert!(control.check().is_err());
        assert!(host.opened.transport.is_closed());
    });
}
#[test]
fn refresh_expiry_is_terminal_even_when_a_late_future_returns_success() {
    run(|c, h| async move {
        let (mut host, _viewer) = pair(&c, &h).await;
        let control = host.observation().unwrap();
        let mut n = 0;
        let turn = RefreshTurn {
            cx: &h,
            control: &control,
            until: now(&h).unwrap() + 20_000,
            wait: Duration::from_millis(1),
        };
        let result = pump_refresh(
            &mut host.renewal,
            &mut host.opened.transport,
            async {
                // Deliberately block this test-only future's poll across expiry. The
                // production LocalAPI implementation independently applies its gate.
                std::thread::sleep(Duration::from_millis(25));
                Ok(())
            },
            turn,
            &mut || nonce(&mut n),
            &mut no_other,
        )
        .await;
        assert_eq!(result, Err(Error::Expired));
        assert!(control.check().is_err());
        assert!(host.opened.transport.is_closed());
    });
}

#[test]
fn a_lapsed_peer_cannot_be_extended_by_successful_observation_challenges() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair(&c, &h).await;
        let control = host.observation().unwrap();
        if let Peer::Fixture { until, .. } = &mut host.opened.peer {
            *until = now(&h).unwrap() + 120_000;
        } else {
            panic!("test fixture unexpectedly replaced");
        }
        let mut n = 0;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            assert!(Instant::now() < deadline);
            let (result, _) = Box::pin(support::both(
                host.drive(Duration::from_millis(2), || nonce(&mut n), no_other),
                viewer.drive(Duration::from_millis(2), no_other),
            ))
            .await;
            if result.is_err() {
                break;
            }
        }
        // This fixture's refresh intentionally cannot extend its immutable proof.
        // Real admission requires a fresh LocalAPI result before old expiry.
        assert!(host.renewed_until().is_some());
        assert!(control.check().is_err());
        assert!(host.opened.transport.is_closed());
    });
}

#[test]
fn dropping_a_polled_host_turn_closes_authority_without_another_service_call() {
    run(|c, h| async move {
        let (mut host, _viewer) = pair(&c, &h).await;
        let control = host.observation().unwrap();
        let mut task = std::task::Context::from_waker(std::task::Waker::noop());
        let mut n = 0;
        let mut dropped_pending = false;
        for _ in 0..16 {
            let mut drive =
                Box::pin(host.drive(Duration::from_millis(30), || nonce(&mut n), no_other));
            match drive.as_mut().poll(&mut task) {
                Poll::Ready(result) => result.unwrap(),
                Poll::Pending => {
                    drop(drive);
                    dropped_pending = true;
                    break;
                }
            }
        }
        assert!(dropped_pending, "no actual I/O wait was exercised");
        assert!(control.check().is_err());
        assert!(host.opened.transport.is_closed());
    });
}

#[test]
fn actual_negotiated_session_attaches_configuration_while_renewal_continues() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair_with_capabilities(
            &c,
            &h,
            vec![fr_wire::negotiation::Capability {
                name: fr_wire::attachment::CAPABILITY.into(),
                version: 1,
                required: true,
            }],
        )
        .await;
        let binding = fr_wire::decoder::Binding {
            parent: ControlBinding {
                id: 8,
                ..host.binding()
            },
            display: 9,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
        };
        let control = host.observation().unwrap();
        let mut hs = host
            .offer_media_channel(fr_transport::quic::ChannelRequest {
                binding,
                ticket: fr_wire::attachment::Ticket(99),
                timeout: Duration::from_secs(2),
            })
            .unwrap();
        let mut cs = None;
        let mut sequence = 1;
        let until = now(&h).unwrap() + 2_000_000;
        loop {
            assert!(now(&h).unwrap() < until);
            hs.transmit(host.io().unwrap().0, &h, || control.check().is_ok())
                .unwrap();
            if let Some(client) = &mut cs {
                fr_transport::quic::MediaChannel::transmit(
                    client,
                    viewer.io().unwrap().0,
                    &c,
                    || c.checkpoint().is_ok(),
                )
                .unwrap();
            }
            let mut offer = None;
            let (a, b) = Box::pin(support::both(
                host.drive(
                    Duration::from_millis(1),
                    || nonce(&mut sequence),
                    |_, _| Ok(Disposition::Blocked),
                ),
                viewer.drive(Duration::from_millis(1), |route, bytes| {
                    if cs.is_none()
                        && matches!(route, Route::Stream(r) if r.binding==7)
                        && bytes[6..8] == 0x001b_u16.to_be_bytes()
                    {
                        assert!(offer.is_none());
                        offer = Some(bytes.to_vec());
                        Ok(Disposition::Consumed)
                    } else {
                        Ok(Disposition::Blocked)
                    }
                }),
            ))
            .await;
            a.unwrap();
            b.unwrap();
            if let Some(bytes) = offer {
                cs = Some(
                    viewer
                        .accept_media_channel(&bytes, Duration::from_secs(2))
                        .unwrap(),
                );
            }
            hs.dispatch(host.io().unwrap().0, &h, || control.check().is_ok())
                .unwrap();
            if let Some(client) = &mut cs {
                client
                    .dispatch(viewer.io().unwrap().0, &c, || c.checkpoint().is_ok())
                    .unwrap();
                let a = hs
                    .finish(host.io().unwrap().0, &h, || control.check().is_ok())
                    .unwrap();
                let b = client
                    .finish(viewer.io().unwrap().0, &c, || c.checkpoint().is_ok())
                    .unwrap();
                if let (Some(a), Some(b)) = (a, b) {
                    assert_eq!(a.descriptor, b.descriptor);
                    assert_eq!(a.outbound.stream, b.inbound.stream);
                    break;
                }
            }
        }
        assert!(host.renewed_until().is_some());
        assert!(control.check().is_ok());
        host.close();
        assert!(control.check().is_err());
    });
}

#[test]
fn display_choice_uses_running_sessions_while_observation_renews_past_initial_grant() {
    run(|c, h| async move {
        use fr_wire::display::{Catalog, Display};
        let (mut host, mut viewer) = pair_with_capabilities(
            &c,
            &h,
            vec![fr_wire::negotiation::Capability {
                name: fr_wire::display::CAPABILITY.into(),
                version: 1,
                required: true,
            }],
        )
        .await;
        let output = Display {
            handle: 91,
            geometry: DisplayGeometryGeneration::INITIAL,
            x: -320,
            y: 0,
            pixel_width: 320,
            pixel_height: 240,
            logical_width: 320,
            logical_height: 240,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
        };
        let catalog = Catalog::new(1, &[output], &host.selection().limits).unwrap();
        let control = host.observation().unwrap();
        let initial = control
            .deadline(Duration::from_secs(3))
            .unwrap()
            .time()
            .as_nanos()
            / 1000;
        let mut hs = host
            .select_display(catalog, Duration::from_secs(5))
            .unwrap();
        let mut vs = viewer.select_display(Duration::from_secs(5)).unwrap();
        let mut sequence = 0;
        // A human can pause on the catalog. Keep the same parent driver alive
        // across actual renewals instead of using selection traffic as renewal.
        while now(&h).unwrap() < initial + 50_000 {
            hs.transmit(host.io().unwrap().0).unwrap();
            let (a, b) = Box::pin(support::both(
                host.drive(
                    Duration::from_millis(5),
                    || nonce(&mut sequence),
                    |_, _| Ok(Disposition::Blocked),
                ),
                viewer.drive(Duration::from_millis(5), |_, _| Ok(Disposition::Blocked)),
            ))
            .await;
            a.unwrap();
            b.unwrap();
            hs.dispatch(host.io().unwrap().0).unwrap();
            vs.dispatch(viewer.io().unwrap().0).unwrap();
        }
        assert!(host.renewed_until().unwrap().as_micros() > initial);
        assert_eq!(vs.catalog(viewer.io().unwrap().0).unwrap(), Some(&catalog));
        assert!(!hs.is_complete());
        assert!(!vs.is_complete());
        vs.choose(viewer.io().unwrap().0, output.handle).unwrap();
        while !hs.is_complete() {
            vs.transmit(viewer.io().unwrap().0).unwrap();
            let (a, b) = Box::pin(support::both(
                host.drive(
                    Duration::from_millis(2),
                    || nonce(&mut sequence),
                    |_, _| Ok(Disposition::Blocked),
                ),
                viewer.drive(Duration::from_millis(2), |_, _| Ok(Disposition::Blocked)),
            ))
            .await;
            a.unwrap();
            b.unwrap();
            hs.dispatch(host.io().unwrap().0).unwrap();
        }
        let hs = hs.finish(host.io().unwrap().0).unwrap();
        let vs = vs.finish(viewer.io().unwrap().0).unwrap();
        let expected = hs.binding(host.io().unwrap().0, 8).unwrap();
        vs.check_binding(viewer.io().unwrap().0, expected).unwrap();
        assert!(control.check().is_ok());
        drop(hs);
        assert!(control.check().is_err());
        assert!(host.check().is_err());
    });
}
