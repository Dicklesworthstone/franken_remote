//! Public observer bootstrap on real localhost UDP/TLS. Host identity and local
//! catalog are fixtures; these ordinary cases do not claim a native decoder.
use super::*;
use crate::session_startup::{
    Configuration, Host, Peer,
    running::{HostSession, tests::run},
    tests::support,
};
use fr_core::{authority::AuthorityPolicy, ids::*, limits::ProtocolLimits};
use fr_wire::{
    display,
    negotiation::{Capability, ControlBinding, Offer, Role},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn capabilities() -> Vec<Capability> {
    let mut caps: Vec<_> = [
        display::CAPABILITY,
        fr_wire::decoder::CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
    ]
    .into_iter()
    .map(|name| Capability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect();
    caps.sort_by(|a, b| a.name.cmp(&b.name));
    caps
}
async fn initial(c: &Cx, h: &Cx, approval: bool) -> (Host, Viewer) {
    let cfg = Configuration {
        offer: Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role: Role::Observe,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities: capabilities(),
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
        transport: quic::Policy::default(),
    };
    let (client, host) = support::native_pair(c, "localhost", quic::ALPN).await;
    let viewer = Viewer::new(
        c.clone(),
        client.unwrap(),
        cfg.offer.clone(),
        cfg.transport,
        cfg.startup_timeout,
    )
    .unwrap();
    let host = Host::start(
        h.clone(),
        host.unwrap(),
        Peer::Fixture {
            alive: Arc::new(AtomicBool::new(true)),
            until: now(h).unwrap() + 30_000_000,
            control: false,
        },
        cfg,
    )
    .unwrap();
    (host, viewer)
}
fn no_launch() -> Launch {
    Launch::new(
        std::path::Path::new("/usr/bin/false"),
        ":0",
        None,
        fr_media::worker::Role::Present,
        81,
    )
    .unwrap()
}
fn catalog() -> Catalog {
    Catalog::new(
        1,
        &[Display {
            handle: 9,
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
        }],
        &ProtocolLimits::ABSOLUTE,
    )
    .unwrap()
}
async fn ready_host(mut host: Host) -> HostSession {
    while !host.is_complete() {
        host.drive(Duration::from_millis(1)).await.unwrap();
    }
    host.finish().unwrap().into_running().unwrap()
}
async fn host_turn(host: &mut HostSession, n: &mut u128) -> Result<(), super::super::Error> {
    host.drive(
        Duration::from_millis(2),
        || {
            *n += 1;
            Ok(*n)
        },
        retain,
    )
    .await
}
async fn selected_host(host: &mut HostSession, n: &mut u128) -> SelectedDisplay {
    let mut choice = host
        .select_display(catalog(), Duration::from_secs(5))
        .unwrap();
    loop {
        choice.transmit(host.io().unwrap().0).unwrap();
        choice.dispatch(host.io().unwrap().0).unwrap();
        if choice.is_complete() {
            return choice.finish(host.io().unwrap().0).unwrap();
        }
        host_turn(host, n).await.unwrap();
    }
}
#[test]
fn invalid_and_unpolled_attempts_cancel_only_their_original_viewer_scope() {
    for invalid in [false, true] {
        run(|c, h| async move {
            let (_host, viewer) = initial(&c, &h, false).await;
            let policy = if invalid {
                Policy {
                    timeout: Duration::ZERO,
                    ..Policy::default()
                }
            } else {
                Policy::default()
            };
            let attempt = viewer.observe(
                no_launch(),
                policy,
                |_| panic!("not reached"),
                |_| panic!("not reached"),
            );
            if invalid {
                assert!(matches!(attempt.await, Err(Error::InvalidConfiguration)));
            } else {
                drop(attempt);
            }
            assert!(c.checkpoint().is_err());
            assert!(h.checkpoint().is_ok());
        });
    }
}
#[test]
fn parked_attempt_uses_its_call_time_deadline_before_any_network_or_native_work() {
    run(|c, h| async move {
        let (_host, viewer) = initial(&c, &h, false).await;
        let attempt = viewer.observe(
            no_launch(),
            Policy {
                timeout: Duration::from_millis(10),
                ..Policy::default()
            },
            |_| panic!("not reached"),
            |_| panic!("not reached"),
        );
        asupersync::time::sleep(c.now(), Duration::from_millis(20)).await;
        assert!(matches!(attempt.await, Err(Error::Expired)));
        assert!(c.checkpoint().is_err());
        assert!(h.checkpoint().is_ok());
    });
}
#[test]
fn approval_notice_does_not_approve_or_open_the_display_catalog() {
    run(|c, h| async move {
        let (mut host, viewer) = initial(&c, &h, true).await;
        let notices = Arc::new(AtomicUsize::new(0));
        let seen = notices.clone();
        let attempt = viewer.observe(
            no_launch(),
            Policy {
                timeout: Duration::from_millis(120),
                ..Policy::default()
            },
            |_| panic!("catalog before approval"),
            move |_| {
                seen.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        );
        let drive = async {
            while c.checkpoint().is_ok() {
                if host.drive(Duration::from_millis(2)).await.is_err() {
                    break;
                }
                assert!(!host.is_complete());
                assert!(host.observation_until.is_none());
            }
        };
        let (result, ()) = Box::pin(support::both(attempt, drive)).await;
        assert!(result.is_err());
        assert_eq!(notices.load(Ordering::Relaxed), 1);
    });
}
#[test]
fn pending_local_display_choice_services_renewal_without_implicit_selection() {
    run(|c, h| async move {
        let (host, viewer) = initial(&c, &h, false).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let attempt = viewer.observe(
            no_launch(),
            Policy {
                timeout: Duration::from_millis(3200),
                ..Policy::default()
            },
            move |_| {
                count.fetch_add(1, Ordering::Relaxed);
                Ok(None)
            },
            |_| Ok(()),
        );
        let serve = async {
            let mut host = Box::pin(ready_host(host)).await;
            let control = host.observation().unwrap();
            let initial = control.deadline(Duration::from_secs(3)).unwrap().time();
            let mut selection = host
                .select_display(catalog(), Duration::from_secs(5))
                .unwrap();
            let mut n = 4000;
            let mut renewed = false;
            while c.checkpoint().is_ok() {
                if selection.transmit(host.io().unwrap().0).is_err() {
                    break;
                }
                if selection.dispatch(host.io().unwrap().0).is_err() {
                    break;
                }
                assert!(!selection.is_complete());
                renewed |= control.deadline(Duration::from_secs(3)).unwrap().time() != initial;
                if host_turn(&mut host, &mut n).await.is_err() {
                    break;
                }
            }
            renewed
        };
        let (result, renewed) = Box::pin(support::both(attempt, serve)).await;
        assert!(result.is_err());
        assert!(renewed);
        assert!(calls.load(Ordering::Relaxed) > 1);
    });
}
#[test]
fn foreign_display_binding_is_rejected_before_attaching_or_launching_decoder() {
    run(|c, h| async move {
        let (host, viewer) = initial(&c, &h, false).await;
        let attempt = viewer.observe(no_launch(), Policy::default(), |_| Ok(Some(9)), |_| Ok(()));
        let serve = async {
            let mut host = Box::pin(ready_host(host)).await;
            let mut n = 5000;
            let selected = selected_host(&mut host, &mut n).await;
            let mut binding = selected.binding(host.io().unwrap().0, 18).unwrap();
            binding.display = 10;
            let mut channel = host
                .offer_media_role(
                    quic::ChannelRequest {
                        binding,
                        ticket: attachment::Ticket(1018),
                        timeout: Duration::from_secs(2),
                    },
                    MediaRole::Configuration,
                )
                .unwrap();
            while c.checkpoint().is_ok() {
                if channel.transmit(host.io().unwrap().0, &h, || true).is_err() {
                    break;
                }
                if host_turn(&mut host, &mut n).await.is_err() {
                    break;
                }
            }
            assert!(!channel.is_complete());
        };
        let (result, ()) = Box::pin(support::both(attempt, serve)).await;
        assert!(matches!(result, Err(Error::Display(_))), "{result:?}");
        assert!(c.checkpoint().is_err());
    });
}
#[test]
fn bounded_slot_cannot_replace_or_grow_for_second_control_record() {
    use fr_transport::quic::StreamRoute;
    let route = Route::Stream(StreamRoute {
        stream: asupersync::net::StreamId(3),
        binding: 7,
        maximum: 128,
        messages: quic::Messages::SessionControl,
        priority: quic::Priority::Critical,
        outbound: false,
    });
    let mut slot = RecordSlot::new(16, route, Kind::StreamBinding).unwrap();
    let mut a = [0_u8; 16];
    a[6..8].copy_from_slice(&(Kind::StreamBinding as u16).to_be_bytes());
    assert_eq!(slot.receive(route, &a), Ok(Disposition::Consumed));
    let mut b = a;
    b[15] = 3;
    assert_eq!(slot.receive(route, &b), Ok(Disposition::Blocked));
    assert_eq!(&slot.bytes[..slot.len], &a);
    assert_eq!(slot.bytes.len(), 16);
}
