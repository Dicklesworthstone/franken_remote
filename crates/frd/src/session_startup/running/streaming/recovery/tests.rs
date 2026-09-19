//! Both canonical host and viewer loops, actual TLS/UDP and supervised child IPC.
//! Only the source/decoder payloads are fixtures; no hardware/HEVC claim is made.
use super::*;
use crate::session_startup::running::{controlled::tests::attach, tests::run};
use crate::session_startup::tests::support;
use crate::{
    media::{Presenter, streaming::Policy},
    session_startup::{self, ViewerSession, viewer::streaming::StreamingViewer},
    worker::{Deadline, Launch},
};
use fr_core::{authority::AuthorityPolicy, ids::*, limits::ProtocolLimits};
use fr_media::{
    delivery::{MediaBudget, ReceivePipeline, ReceivePolicy, SendPolicy},
    worker::{Backend, Configuration, Role as WorkerRole},
};
use fr_wire::{
    Channel,
    attachment::MediaRole,
    negotiation::{Capability, ControlBinding, Offer, Role},
};
use std::{
    cell::RefCell,
    os::unix::fs::PermissionsExt,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

fn capabilities(enabled: bool) -> Vec<Capability> {
    let mut caps: Vec<_> = [
        fr_wire::decoder::CAPABILITY,
        fr_wire::attachment::CAPABILITY,
        fr_wire::attachment::DELIVERY_CAPABILITY,
        fr_wire::receiver_metrics::CAPABILITY,
    ]
    .into_iter()
    .map(|name| Capability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect();
    if enabled {
        caps.push(Capability {
            name: recovery_request::CAPABILITY.into(),
            version: 1,
            required: true,
        });
    }
    caps.sort_by(|a, b| a.name.cmp(&b.name));
    caps
}
async fn pair(c: &Cx, h: &Cx, role: Role, enabled: bool) -> (HostSession, ViewerSession) {
    let cfg = session_startup::Configuration {
        offer: Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities: capabilities(enabled),
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
        transport: fr_transport::quic::Policy {
            critical_send_records: 1,
            ..fr_transport::quic::Policy::default()
        },
    };
    let (client, server) = support::native_pair(c, "localhost", fr_transport::quic::ALPN).await;
    let mut viewer = session_startup::Viewer::new(
        c.clone(),
        client.unwrap(),
        cfg.offer.clone(),
        cfg.transport,
        Duration::from_secs(2),
    )
    .unwrap();
    let mut host = session_startup::Host::start(
        h.clone(),
        server.unwrap(),
        session_startup::Peer::Fixture {
            alive: Arc::new(AtomicBool::new(true)),
            until: now(h).unwrap() + 30_000_000,
            control: true,
        },
        cfg,
    )
    .unwrap();
    while !viewer.is_complete() || !host.is_complete() {
        let (a, b) = Box::pin(support::both(
            host.drive(Duration::from_millis(1)),
            viewer.drive(Duration::from_millis(1)),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
    (
        host.finish().unwrap().into_running().unwrap(),
        viewer.finish().unwrap(),
    )
}
fn configuration() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
async fn source(control: &ObservationControl, mode: &str) -> CaptureSource {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-auto-host-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(
        &path,
        include_str!("capture_fixture.py").replace("@MODE@", mode),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    CaptureSource::start(
        control,
        Launch::new(&path, ":0", None, WorkerRole::Capture, 1).unwrap(),
        configuration(),
    )
    .await
    .unwrap()
}
struct Fixture {
    host: StreamingHost,
    viewer: ViewerSession,
    media: NegotiatedMedia,
    receiver: ReceivePipeline,
    presenter: Presenter,
    initial: crate::media::PresentationReceipt,
}
#[allow(clippy::too_many_lines)]
async fn fixture(c: &Cx, h: &Cx, mode: &str, budget: u64, role: Role, enabled: bool) -> Fixture {
    let (mut host, mut viewer) = pair(c, h, role, enabled).await;
    let (hc, vc) = attach(&mut host, &mut viewer, c, h, MediaRole::Configuration, 18).await;
    let (hr, vr) = attach(&mut host, &mut viewer, c, h, MediaRole::Recovery, 19).await;
    let (hv, vv) = attach(&mut host, &mut viewer, c, h, MediaRole::Video, 20).await;
    let selected = host.selection().clone();
    let hm = NegotiatedMedia::new(host.io().unwrap().0, &selected, &hc, &hr, &hv).unwrap();
    let vm = NegotiatedMedia::new(viewer.io().unwrap().0, &selected, &vc, &vr, &vv).unwrap();
    let control = host.observation().unwrap();
    let mut source = source(&control, mode).await;
    let mut sender = hm
        .sender(
            host.io().unwrap().0,
            control.clone(),
            SendPolicy {
                recovery_horizon_micros: budget,
                ..SendPolicy::default()
            },
        )
        .unwrap();
    let config = vm
        .receiver_config(
            viewer.io().unwrap().0,
            ReceivePolicy {
                reference_budget_micros: 120_000,
                recovery_budget_micros: budget,
                ..ReceivePolicy::default()
            },
        )
        .unwrap();
    let mut receiver =
        ReceivePipeline::new(config, MediaBudget::new(config.limits.protocol()).unwrap()).unwrap();
    let mut presenter =
        Presenter::stream_fixture(c, viewer.io().unwrap().0, &vm, &mut receiver).await;
    sender
        .enqueue_capture(source.capture_if_changed(&control, true).await.unwrap())
        .unwrap();
    let mut entropy = 1000;
    let initial = loop {
        sender
            .transmit(h, host.io().unwrap().0, Lane::Original)
            .unwrap();
        let (a, b) = Box::pin(support::both(
            host.drive(
                Duration::from_millis(1),
                || super::super::tests::nonce(&mut entropy),
                super::super::tests::block,
            ),
            viewer.drive(Duration::from_millis(1), super::super::tests::block),
        ))
        .await;
        a.unwrap();
        b.unwrap();
        vm.receive_ready(
            c,
            viewer.io().unwrap().0,
            || true,
            |channel, bytes| {
                receiver.receive(channel, bytes, now(c).unwrap()).unwrap();
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
        if let Some(receipt) = presenter.present_next(c, &mut receiver).await.unwrap() {
            break receipt;
        }
    };
    // Lose an actual sender reference before handing both peers to their normal
    // running loops. No test host drives the recovery handshake or creates a
    // replacement sender. Every remaining recovery stage is production code.
    sender
        .enqueue_capture(source.capture_if_changed(&control, false).await.unwrap())
        .unwrap();
    let mut announced = false;
    let mut lost = false;
    while !announced || !lost {
        for _ in 0..4 {
            sender
                .transmit(h, host.io().unwrap().0, Lane::Original)
                .unwrap();
        }
        let (a, b) = Box::pin(support::both(
            host.drive(
                Duration::from_millis(1),
                || super::super::tests::nonce(&mut entropy),
                super::super::tests::block,
            ),
            viewer.drive(Duration::from_millis(1), super::super::tests::block),
        ))
        .await;
        a.unwrap();
        b.unwrap();
        vm.receive_ready(
            c,
            viewer.io().unwrap().0,
            || true,
            |channel, bytes| {
                if channel == Channel::Video {
                    lost = true;
                } else {
                    receiver.receive(channel, bytes, now(c).unwrap()).unwrap();
                    announced = true;
                }
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
    }
    let stream = Stream {
        source,
        sender,
        control,
        policy: Policy::default(),
        capacity: configuration().max_access_unit_bytes as usize
            + fr_media::worker::UNIT_PREFIX_BYTES,
        statistics: Statistics::default(),
        served: false,
        pacing: None,
    };
    let mut host = host.into_streaming(stream).unwrap();
    let enable = host.enable_reference_recovery(hm);
    if enabled && role == Role::Observe {
        enable.unwrap();
    } else {
        assert!(enable.is_err());
    }
    Fixture {
        host,
        viewer,
        media: vm,
        receiver,
        presenter,
        initial,
    }
}

#[test]
fn both_running_peers_recover_and_continue_on_the_original_workers_and_connection() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let Fixture {
            mut host,
            viewer,
            media,
            receiver,
            presenter,
            initial,
        } = Box::pin(fixture(&c, &h, "healthy", 2_000_000, Role::Observe, true)).await;
        let original_host = host.worker_id();
        let original_viewer = presenter.worker_id();
        let original_connection = host.host.session().unwrap().opened.transport.binding();
        let control = host.stream.control.clone();
        let mut viewer =
            StreamingViewer::from_test_parts(viewer, media, presenter, receiver, initial);
        // A loss burst plus receive silence outlives the reference deadline.
        // Start the canonical loop only after this real timer elapsed, so its
        // normal selective repair cannot make the intentionally lost frame whole.
        asupersync::time::sleep(c.now(), Duration::from_millis(150)).await;
        let stop = viewer.control();
        let frames = Rc::new(RefCell::new(Vec::new()));
        let record = frames.clone();
        let start = Instant::now();
        let mut entropy = 5000;
        let (server, client) = Box::pin(support::both(
            host.serve(
                || super::super::tests::nonce(&mut entropy),
                || None,
                super::super::tests::block,
            ),
            viewer.serve(
                |_, event| {
                    if let Some(event) = event {
                        record.borrow_mut().push(event.frame.as_raw());
                    }
                    if start.elapsed() > Duration::from_millis(3300) && record.borrow().len() > 4 {
                        stop.stop();
                    }
                    Ok(())
                },
                |_| {},
                super::super::tests::block,
            ),
        ))
        .await;
        assert!(server.is_err());
        assert!(client.is_err());
        assert_eq!(host.worker_id(), original_host);
        assert_eq!(viewer.worker_id(), original_viewer);
        assert_eq!(host.statistics().recovered_streams, 1);
        assert_eq!(
            viewer.statistics().recovered_streams,
            1,
            "server={server:?} client={client:?} frames={:?}",
            frames.borrow()
        );
        assert!(
            host.host
                .session()
                .unwrap()
                .opened
                .transport
                .is_bound_to(&original_connection)
        );
        assert!(
            frames.borrow().contains(&3),
            "the obsolete in-flight picture 2 must drain before the recovery IDR 3"
        );
        assert!(!frames.borrow().contains(&1));
        assert!(!frames.borrow().contains(&2));
        assert!(frames.borrow().contains(&4));
        assert!(control.check().is_err());
        assert!(original_viewer.is_some());
        host.reap_media(
            &cleanup,
            Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        viewer
            .reap_media(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
    });
}

fn failure(mode: &'static str, budget: u64) {
    run(move |c, h| async move {
        let cleanup = Cx::current().unwrap();
        let Fixture {
            mut host,
            viewer,
            media,
            receiver,
            presenter,
            initial,
        } = Box::pin(fixture(&c, &h, mode, budget, Role::Observe, true)).await;
        let mut viewer =
            StreamingViewer::from_test_parts(viewer, media, presenter, receiver, initial);
        // A loss burst plus receive silence outlives the reference deadline.
        // Start the canonical loop only after this real timer elapsed, so its
        // normal selective repair cannot make the intentionally lost frame whole.
        asupersync::time::sleep(c.now(), Duration::from_millis(150)).await;
        let frames = Rc::new(RefCell::new(Vec::new()));
        let seen = frames.clone();
        let mut entropy = 6000;
        let started = Instant::now();
        let (server, client) = Box::pin(support::both(
            host.serve(
                || super::super::tests::nonce(&mut entropy),
                || None,
                super::super::tests::block,
            ),
            viewer.serve(
                |_, event| {
                    if let Some(event) = event {
                        seen.borrow_mut().push(event.frame.as_raw());
                    }
                    Ok(())
                },
                |_| {},
                super::super::tests::block,
            ),
        ))
        .await;
        assert!(server.is_err());
        assert!(client.is_err());
        assert_eq!(viewer.statistics().recovered_streams, 0);
        assert!(
            frames.borrow().iter().all(|&frame| frame == 0),
            "server={server:?} client={client:?} frames={:?}",
            frames.borrow()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        host.reap_media(
            &cleanup,
            Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        viewer
            .reap_media(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
    });
}
#[test]
fn hung_old_capture_cannot_hold_the_failed_session_past_its_recovery_deadline() {
    failure("stall", 400_000);
}
#[test]
fn a_native_worker_ignoring_force_idr_cannot_complete_automatic_recovery() {
    failure("ignore-force", 1_000_000);
}

#[test]
fn missing_negotiation_and_control_intent_cannot_enable_automatic_recovery() {
    for (role, enabled) in [(Role::Observe, false), (Role::RequestControl, true)] {
        run(move |c, h| async move {
            let cleanup = Cx::current().unwrap();
            let Fixture {
                mut host,
                mut presenter,
                viewer,
                ..
            } = Box::pin(fixture(&c, &h, "healthy", 2_000_000, role, enabled)).await;
            assert!(host.recovery.is_none());
            assert!(host.stream.control.check().is_ok());
            drop(viewer);
            host.reap_media(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
            presenter.abort();
            presenter
                .reap(
                    &cleanup,
                    Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
                )
                .await
                .unwrap();
        });
    }
}
