#![cfg(target_os = "linux")]
//! Actual UDP/TLS with explicit approved-authority fixtures. No native capture,
//! production entropy, Tailscale policy, or graphical consent is simulated as real.
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod network;
use asupersync::{cx::Cx, net::quic_native::NativeQuicUdpConnection, types::Budget};
use fr_client::{authority::ObservationResponder, input::ClientInstant};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::RemoteSessionId,
    limits::ProtocolLimits,
    time::HostDuration,
};
use fr_transport::quic::{
    ALPN, ControlRoutes, Disposition, Messages, Policy, Priority, QuicRecords, Route, StreamRoute,
};
use fr_wire::{
    authority::{self, Binding, Message, Scope},
    input::{InputDelivery, InputDirection},
};
use frd::media::{
    ObservationControl, host_now,
    renewal::{Error, Event, ObservationRenewal},
};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;
fn binding() -> Binding {
    Binding {
        channel: 17,
        session: RemoteSessionId::from_raw(0xabcd),
    }
}
fn approved(cx: Cx, lifetime: u64) -> ObservationControl {
    let mut a = SessionAuthority::new(
        binding().session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(lifetime),
            ticket_lifetime: HostDuration::from_micros(1_000_000),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.require_approval().unwrap();
    a.authorize_observation(host_now(&cx).unwrap()).unwrap();
    ObservationControl::new(cx, a).unwrap()
}
async fn native(cx: &Cx) -> (NativeQuicUdpConnection, QuicRecords, ControlRoutes) {
    let (c, s) = network::native_pair(cx, "localhost", ALPN).await;
    let (mut c, mut s) = (c.unwrap(), s.unwrap());
    let inbound = StreamRoute {
        stream: c.connection_mut().open_uni_stream(cx).unwrap(),
        binding: binding().channel,
        messages: Messages::SessionControl,
        priority: Priority::Critical,
        maximum: 1024,
        outbound: false,
    };
    let outbound = StreamRoute {
        stream: s.connection_mut().open_uni_stream(cx).unwrap(),
        outbound: true,
        ..inbound
    };
    let policy = Policy {
        critical_send_records: 1,
        ..Policy::default()
    };
    let s = QuicRecords::new(s, cx, &[inbound, outbound], &[], policy).unwrap();
    (c, s, ControlRoutes { inbound, outbound })
}
async fn pair(cx: &Cx) -> (QuicRecords, QuicRecords, ControlRoutes) {
    let (c, s, routes) = native(cx).await;
    let c = QuicRecords::new(
        c,
        cx,
        &[
            StreamRoute {
                outbound: true,
                ..routes.inbound
            },
            StreamRoute {
                outbound: false,
                ..routes.outbound
            },
        ],
        &[],
        Policy::default(),
    )
    .unwrap();
    (c, s, routes)
}
async fn drive(cx: &Cx, c: &mut QuicRecords, s: &mut QuicRecords, owner: &ObservationRenewal) {
    let (a, b) = Box::pin(network::both(
        c.drive(cx, Duration::from_millis(2), || true),
        owner.drive(s, Duration::from_millis(2)),
    ))
    .await;
    a.unwrap();
    b.unwrap();
}
fn reply(nonce: u128, scope: Scope, channel: u32) -> Vec<u8> {
    let mut bytes = [0; 82];
    let n = authority::encode(
        Message::Response { scope, nonce },
        Binding {
            channel,
            ..binding()
        },
        &LIMITS,
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes[..n].to_vec()
}
fn send(cx: &Cx, c: &mut QuicRecords, routes: ControlRoutes, bytes: &[u8]) {
    c.send(
        cx,
        Route::Stream(StreamRoute {
            outbound: true,
            ..routes.inbound
        }),
        bytes,
        network::clock(cx) + 900_000,
        || true,
    )
    .unwrap();
}
fn challenge(cx: &Cx, c: &mut QuicRecords, routes: ControlRoutes) -> Option<(u128, u64)> {
    let mut result = None;
    c.receive(
        cx,
        || true,
        |route, bytes| {
            assert_eq!(
                route,
                Route::Stream(StreamRoute {
                    outbound: false,
                    ..routes.outbound
                })
            );
            let Message::Challenge {
                scope: Scope::Observation,
                nonce,
                deadline_micros,
            } = authority::decode(
                bytes,
                binding(),
                &LIMITS,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
            .unwrap()
            else {
                panic!("observation challenge required")
            };
            result = Some((nonce, deadline_micros));
            Ok(Disposition::Consumed)
        },
    )
    .unwrap();
    result
}
#[test]
fn real_viewer_responses_keep_observation_past_initial_expiry_with_issue_time_deadlines() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (mut c, mut s, routes) = pair(&cx).await;
        let control = approved(hc, 3_000_000);
        let initial = control.deadline(Duration::from_secs(3)).unwrap().time();
        let mut owner = ObservationRenewal::new(control.clone(), &s, routes, LIMITS).unwrap();
        let mut viewer = ObservationResponder::new(binding(), LIMITS, ClientInstant(0)).unwrap();
        let mut count = 0;
        let mut last_deadline = 0;
        let until = network::clock(&cx) + 3_250_000;
        while network::clock(&cx) < until {
            owner
                .service(&mut s, || {
                    count += 1;
                    Ok(count)
                })
                .unwrap();
            drive(&cx, &mut c, &mut s, &owner).await;
            c.receive(
                &cx,
                || true,
                |_, bytes| {
                    let Message::Challenge {
                        deadline_micros, ..
                    } = authority::decode(
                        bytes,
                        binding(),
                        &LIMITS,
                        InputDirection::HostToViewer,
                        InputDelivery::Reliable,
                    )
                    .unwrap()
                    else {
                        panic!("challenge")
                    };
                    last_deadline = deadline_micros;
                    viewer
                        .accept(bytes, ClientInstant(network::clock(&cx)))
                        .unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
            let now = network::clock(&cx);
            if let Some(bytes) = viewer.pending(ClientInstant(now)).unwrap() {
                send(&cx, &mut c, routes, bytes);
                viewer.sent(ClientInstant(now)).unwrap();
            }
            drive(&cx, &mut c, &mut s, &owner).await;
            owner.receive(&mut s, |_, _| Err(())).unwrap();
            if let Some(until) = owner.renewed_until() {
                assert!(until.as_micros() <= last_deadline);
                assert_eq!(
                    control
                        .deadline(Duration::from_secs(3))
                        .unwrap()
                        .time()
                        .as_nanos()
                        / 1000,
                    until.as_micros()
                );
            }
        }
        assert!(
            cx.now() > initial,
            "cross the original application expiration"
        );
        assert!(control.check().is_ok());
        assert!(count >= 4);
        assert_eq!(owner.renewed_until().unwrap().as_micros(), last_deadline);
        drop(owner);
        assert!(control.check().is_err());
    });
}
#[test]
fn delayed_response_does_not_reset_the_host_deadline_or_bypass_the_old_expiry() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (mut c, mut s, routes) = pair(&cx).await;
        let control = approved(hc, 3_000_000);
        let mut owner = ObservationRenewal::new(control.clone(), &s, routes, LIMITS).unwrap();
        owner.service(&mut s, || Ok(99)).unwrap();
        let mut got = None;
        while got.is_none() {
            drive(&cx, &mut c, &mut s, &owner).await;
            got = challenge(&cx, &mut c, routes);
        }
        let (nonce, deadline) = got.unwrap();
        asupersync::time::sleep(cx.now(), Duration::from_millis(120)).await;
        send(&cx, &mut c, routes, &reply(nonce, Scope::Observation, 17));
        while owner.renewed_until().is_none() {
            drive(&cx, &mut c, &mut s, &owner).await;
            owner.receive(&mut s, |_, _| Err(())).unwrap();
        }
        assert_eq!(owner.renewed_until().unwrap().as_micros(), deadline);
        assert!(
            control
                .deadline(Duration::from_secs(3))
                .unwrap()
                .time()
                .as_nanos()
                / 1000
                < network::clock(&cx) + 2_950_000
        );
        // A consumed response cannot answer any subsequent challenge.
        send(&cx, &mut c, routes, &reply(nonce, Scope::Observation, 17));
        let until = network::clock(&cx) + 500_000;
        loop {
            assert!(network::clock(&cx) < until);
            drive(&cx, &mut c, &mut s, &owner).await;
            if let Err(error) = owner.receive(&mut s, |_, _| Err(())) {
                assert_eq!(error, Error::UnexpectedResponse);
                break;
            }
        }
        assert!(s.is_closed());
        assert!(control.check().is_err());
    });
}
#[test]
fn queued_challenge_and_network_ack_are_not_renewal_and_late_response_cannot_revive() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (mut c, mut s, routes) = pair(&cx).await;
        let control = approved(hc, 150_000);
        let mut owner = ObservationRenewal::new(control.clone(), &s, routes, LIMITS).unwrap();
        owner.service(&mut s, || Ok(10)).unwrap();
        let mut got = None;
        while got.is_none() {
            drive(&cx, &mut c, &mut s, &owner).await;
            got = challenge(&cx, &mut c, routes);
        }
        asupersync::time::sleep(cx.now(), Duration::from_millis(160)).await;
        let called = AtomicUsize::new(0);
        assert!(
            owner
                .service(&mut s, || {
                    called.fetch_add(1, Ordering::Relaxed);
                    Ok(11)
                })
                .is_err()
        );
        assert_eq!(called.load(Ordering::Relaxed), 0);
        assert!(s.is_closed());
        assert!(control.renew(10).is_err());
        assert!(owner.renewed_until().is_none());
    });
}
#[test]
fn native_send_backpressure_preserves_nonce_bytes_and_fixed_expiry() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (mut c, mut s, routes) = pair(&cx).await;
        // Occupy the one critical record slot with a separate framed control
        // marker. This is not renewal and its opaque body is not OS input.
        let mut marker = reply(200, Scope::Observation, 17);
        marker[7] = 0x12;
        s.send(
            &cx,
            Route::Stream(routes.outbound),
            &marker,
            network::clock(&cx) + 2_000_000,
            || true,
        )
        .unwrap();
        let control = approved(hc, 3_000_000);
        let mut owner = ObservationRenewal::new(control.clone(), &s, routes, LIMITS).unwrap();
        assert_eq!(
            owner.service(&mut s, || Ok(66)).unwrap(),
            Event::Backpressure
        );
        for _ in 0..8 {
            assert_eq!(
                owner
                    .service(&mut s, || panic!("must not replace pending challenge"))
                    .unwrap(),
                Event::Backpressure
            );
        }
        let before = network::clock(&cx);
        let until = before + 500_000;
        let mut result = None;
        while result.is_none() {
            assert!(network::clock(&cx) < until);
            drive(&cx, &mut c, &mut s, &owner).await;
            c.receive(
                &cx,
                || true,
                |_, bytes| {
                    if bytes[7] == 0x15 {
                        result = Some(
                            authority::decode(
                                bytes,
                                binding(),
                                &LIMITS,
                                InputDirection::HostToViewer,
                                InputDelivery::Reliable,
                            )
                            .unwrap(),
                        );
                    } else {
                        assert_eq!(bytes, &marker);
                    }
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
            owner
                .service(&mut s, || panic!("one outstanding nonce"))
                .unwrap();
        }
        let Some(Message::Challenge {
            nonce,
            deadline_micros,
            ..
        }) = result
        else {
            panic!("challenge")
        };
        assert_eq!(nonce, 66);
        assert!(deadline_micros <= before + 3_000_000);
        assert!(owner.renewed_until().is_none());
    });
}
#[test]
fn duplicate_attachment_and_foreign_connection_never_take_over_an_existing_owner() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (_, s, routes) = pair(&cx).await;
        let (_, mut replacement, _) = pair(&cx).await;
        let control = approved(hc, 3_000_000);
        let mut owner = ObservationRenewal::new(control.clone(), &s, routes, LIMITS).unwrap();
        assert!(matches!(
            ObservationRenewal::new(control.clone(), &s, routes, LIMITS),
            Err(Error::AlreadyAttached)
        ));
        assert!(control.check().is_ok());
        assert_eq!(
            owner.service(&mut replacement, || Ok(1)),
            Err(Error::ForeignConnection)
        );
        assert!(control.check().is_err());
        assert!(!replacement.is_closed());
    });
}
#[test]
fn dropping_unpolled_io_or_panicking_nonce_source_revokes_before_connection_close() {
    for panic_nonce in [false, true] {
        let rt = network::runtime();
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let hc = rt.request_cx_with_budget(Budget::INFINITE);
        rt.block_on(async {
            let (_, mut s, routes) = pair(&cx).await;
            let control = approved(hc, 3_000_000);
            let mut owner = ObservationRenewal::new(control.clone(), &s, routes, LIMITS).unwrap();
            if panic_nonce {
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _ = owner.service(&mut s, || panic!("fixture entropy failure"));
                    }))
                    .is_err()
                );
            } else {
                drop(owner.drive(&mut s, Duration::from_millis(1)));
            }
            assert!(s.is_closed());
            assert!(control.check().is_err());
        });
    }
}
#[test]
fn peer_fin_or_reset_ends_observation_without_waiting_for_another_application_record() {
    for reset in [false, true] {
        let rt = network::runtime();
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let hc = rt.request_cx_with_budget(Budget::INFINITE);
        rt.block_on(async {
            let (mut peer, mut s, routes) = native(&cx).await;
            let control = approved(hc, 3_000_000);
            let owner = ObservationRenewal::new(control.clone(), &s, routes, LIMITS).unwrap();
            if reset {
                peer.connection_mut()
                    .reset_stream(&cx, routes.inbound.stream, 0)
                    .unwrap();
            } else {
                peer.connection_mut()
                    .write_stream(
                        &cx,
                        routes.inbound.stream,
                        asupersync::bytes::Bytes::new(),
                        true,
                    )
                    .unwrap();
            }
            let until = network::clock(&cx) + 500_000;
            loop {
                assert!(network::clock(&cx) < until);
                let ((), r) = Box::pin(network::both(
                    async {
                        peer.flush(&cx).await.unwrap();
                        let _ = peer.drive_io_once(&cx, Duration::from_millis(1)).await;
                    },
                    owner.drive(&mut s, Duration::from_millis(1)),
                ))
                .await;
                if let Err(e) = r {
                    assert_eq!(e, Error::PeerClosed);
                    break;
                }
            }
            assert!(s.is_closed());
            assert!(control.check().is_err());
        });
    }
}

#[test]
fn an_unsent_challenge_expires_in_place_instead_of_sliding_with_backpressure() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (_, mut s, routes) = pair(&cx).await;
        let mut marker = reply(1, Scope::Observation, 17);
        marker[7] = 0x12;
        s.send(
            &cx,
            Route::Stream(routes.outbound),
            &marker,
            network::clock(&cx) + 2_500_000,
            || true,
        )
        .unwrap();
        let control = approved(hc, 3_000_000);
        let mut owner = ObservationRenewal::new(control.clone(), &s, routes, LIMITS).unwrap();
        assert_eq!(
            owner.service(&mut s, || Ok(2)).unwrap(),
            Event::Backpressure
        );
        asupersync::time::sleep(cx.now(), Duration::from_millis(1020)).await;
        assert!(
            control.check().is_ok(),
            "application authority has not expired yet"
        );
        assert_eq!(
            owner.service(&mut s, || panic!("cannot replace a pending nonce")),
            Err(Error::Expired)
        );
        assert!(control.check().is_err());
        assert!(s.is_closed());
    });
}
#[test]
fn other_control_responses_are_not_observation_renewal_and_bad_nonces_close_the_session() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (mut c, mut s, routes) = pair(&cx).await;
        let control = approved(hc, 3_000_000);
        let mut owner = ObservationRenewal::new(control.clone(), &s, routes, LIMITS).unwrap();
        owner.service(&mut s, || Ok(4)).unwrap();
        let mut got = None;
        while got.is_none() {
            drive(&cx, &mut c, &mut s, &owner).await;
            got = challenge(&cx, &mut c, routes);
        }
        let bytes = reply(
            4,
            Scope::Control(fr_core::ids::InputLeaseId::from_raw(7)),
            17,
        );
        send(&cx, &mut c, routes, &bytes);
        let mut delegated = false;
        let deadline = network::clock(&cx) + 500_000;
        while !delegated {
            assert!(network::clock(&cx) < deadline);
            drive(&cx, &mut c, &mut s, &owner).await;
            owner
                .receive(&mut s, |_, received| {
                    assert_eq!(received, bytes);
                    delegated = true;
                    Ok(Disposition::Consumed)
                })
                .unwrap();
        }
        assert!(owner.renewed_until().is_none());
        assert!(control.check().is_ok());
        send(&cx, &mut c, routes, &reply(5, Scope::Observation, 17));
        loop {
            assert!(network::clock(&cx) < deadline);
            drive(&cx, &mut c, &mut s, &owner).await;
            if let Err(e) = owner.receive(&mut s, |_, _| Err(())) {
                assert_eq!(e, Error::UnexpectedResponse);
                break;
            }
        }
        assert!(control.check().is_err());
        assert!(s.is_closed());
    });
}
#[test]
fn invalid_routes_and_failed_nonce_sources_do_not_create_a_renewal_bypass() {
    for zero in [false, true] {
        let rt = network::runtime();
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let hc = rt.request_cx_with_budget(Budget::INFINITE);
        rt.block_on(async {
            let (c, mut s, routes) = pair(&cx).await;
            let control = approved(hc, 3_000_000);
            assert!(matches!(
                ObservationRenewal::new(control.clone(), &c, routes, LIMITS),
                Err(Error::InvalidRoutes)
            ));
            let wrong = ControlRoutes {
                inbound: StreamRoute {
                    binding: 99,
                    ..routes.inbound
                },
                ..routes
            };
            assert!(matches!(
                ObservationRenewal::new(control.clone(), &s, wrong, LIMITS),
                Err(Error::InvalidRoutes)
            ));
            assert!(control.check().is_ok());
            let mut owner = ObservationRenewal::new(control.clone(), &s, routes, LIMITS).unwrap();
            assert_eq!(
                owner.service(&mut s, || if zero { Ok(0) } else { Err(()) }),
                Err(Error::NonceUnavailable)
            );
            assert!(control.check().is_err());
            assert!(s.is_closed());
        });
    }
}
