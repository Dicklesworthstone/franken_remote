//! Public viewer bootstrap and continuous service with real HEVC/X11. Tailnet
//! identity, host approval and display enumeration remain explicit local fixtures.
use super::*;
use crate::session_startup::{
    Configuration as SessionConfiguration, Host as StartupHost, ObserverPolicy, Peer, Viewer,
};
use fr_core::{authority::AuthorityPolicy, ids::*, limits::ProtocolLimits};
use fr_transport::quic::{ChannelRequest, MediaChannel};
use fr_wire::{
    display::{self, Catalog, Display as RemoteDisplay},
    negotiation::{ControlBinding, Offer, Role as Intent},
};
use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

fn catalog() -> Catalog {
    Catalog::new(
        1,
        &[RemoteDisplay {
            handle: 9,
            geometry: DisplayGeometryGeneration::INITIAL,
            x: 0,
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
async fn startup(c: &Cx, h: &Cx) -> (StartupHost, Viewer) {
    let mut caps = capabilities();
    caps.push(Capability {
        name: display::CAPABILITY.into(),
        version: display::VERSION,
        required: true,
    });
    caps.sort_by(|a, b| a.name.cmp(&b.name));
    let config = SessionConfiguration {
        offer: Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role: Intent::Observe,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities: caps,
        },
        binding: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(13),
        },
        require_approval: true,
        startup_timeout: Duration::from_secs(2),
        authority: AuthorityPolicy::plan_defaults(),
        transport: fr_transport::quic::Policy::default(),
    };
    let (client, host) = support::native_pair(c, "localhost", fr_transport::quic::ALPN).await;
    let viewer = Viewer::new(
        c.clone(),
        client.unwrap(),
        config.offer.clone(),
        config.transport,
        config.startup_timeout,
    )
    .unwrap();
    let host = StartupHost::start(
        h.clone(),
        host.unwrap(),
        Peer::Fixture {
            alive: Arc::new(AtomicBool::new(true)),
            until: now(h).unwrap() + 30_000_000,
            control: false,
        },
        config,
    )
    .unwrap();
    (host, viewer)
}
async fn host_turn(host: &mut HostSession, n: &mut u128) {
    host.drive(Duration::from_millis(2), || nonce(n), block)
        .await
        .unwrap();
}
async fn attach_host(
    host: &mut HostSession,
    selected: &crate::display_selection::SelectedDisplay,
    h: &Cx,
    role: MediaRole,
    id: u32,
    n: &mut u128,
) -> MediaChannel {
    let binding = selected.binding(host.io().unwrap().0, id).unwrap();
    let control = host.observation().unwrap();
    let mut channel = host
        .offer_media_role(
            ChannelRequest {
                binding,
                ticket: attachment::Ticket(u128::from(id) + 100),
                timeout: Duration::from_secs(2),
            },
            role,
        )
        .unwrap();
    loop {
        channel
            .transmit(host.io().unwrap().0, h, || control.check().is_ok())
            .unwrap();
        channel
            .dispatch(host.io().unwrap().0, h, || control.check().is_ok())
            .unwrap();
        if channel
            .finish(host.io().unwrap().0, h, || control.check().is_ok())
            .unwrap()
            .is_some()
        {
            return channel;
        }
        host_turn(host, n).await;
    }
}
struct Hosted {
    streaming: StreamingHost,
    selected: crate::display_selection::SelectedDisplay,
    control: ObservationControl,
    renewal_during_configuration: bool,
    n: u128,
}
#[allow(clippy::too_many_lines)]
async fn host_bootstrap(
    mut host: StartupHost,
    h: &Cx,
    image: &Path,
    source_display: &Display,
    notice: &AtomicUsize,
) -> Hosted {
    while !host.is_complete() {
        host.drive(Duration::from_millis(2)).await.unwrap();
        if notice.load(Ordering::Acquire) != 0
            && let Some(approval) = host.approval()
        {
            assert!(host.observation_until.is_none());
            // Explicit host-side local fixture, not a viewer approval message.
            approval.decide(true).unwrap();
        }
    }
    let mut host = host.finish().unwrap().into_running().unwrap();
    let mut n = 7000;
    let mut selection = host
        .select_display(catalog(), Duration::from_secs(5))
        .unwrap();
    while !selection.is_complete() {
        selection.transmit(host.io().unwrap().0).unwrap();
        selection.dispatch(host.io().unwrap().0).unwrap();
        host_turn(&mut host, &mut n).await;
    }
    let selected = selection.finish(host.io().unwrap().0).unwrap();
    let configuration_channel = attach_host(
        &mut host,
        &selected,
        h,
        MediaRole::Configuration,
        18,
        &mut n,
    )
    .await;
    let recovery = attach_host(&mut host, &selected, h, MediaRole::Recovery, 19, &mut n).await;
    let video = attach_host(&mut host, &selected, h, MediaRole::Video, 20, &mut n).await;
    let selection = host.selection().clone();
    let media = NegotiatedMedia::new(
        host.io().unwrap().0,
        &selection,
        &configuration_channel,
        &recovery,
        &video,
    )
    .unwrap();
    let control = host.observation().unwrap();
    let mut source = CaptureSource::start(
        &control,
        source_display.launch(image, Role::Capture, 141),
        configuration(),
    )
    .await
    .unwrap();
    let first = source.capture_if_changed(&control, true).await.unwrap();
    let setup = selected
        .decoder_setup(host.io().unwrap().0, &media, Duration::from_secs(2))
        .unwrap();
    let mut decoder = decoder_startup::Host::new(
        control.clone(),
        host.io().unwrap().0,
        setup,
        configuration(),
        first,
    )
    .unwrap();
    let original = control.deadline(Duration::from_secs(3)).unwrap().time();
    let mut sender = media
        .sender(host.io().unwrap().0, control.clone(), SendPolicy::default())
        .unwrap();
    let mut sent = false;
    let mut released = false;
    let mut renewed = false;
    while !decoder.is_complete() {
        if !sent {
            sent = decoder.transmit(host.io().unwrap().0).unwrap();
        }
        decoder.dispatch(host.io().unwrap().0).unwrap();
        if !released {
            renewed |= control.deadline(Duration::from_secs(3)).unwrap().time() > original;
            if let Some(update) = decoder.take_recovery().unwrap() {
                sender.enqueue_capture(update).unwrap();
                released = true;
            }
        }
        if released {
            sender
                .transmit(h, host.io().unwrap().0, Lane::Original)
                .unwrap();
        }
        host_turn(&mut host, &mut n).await;
    }
    let stream = Stream::new(
        decoder,
        source,
        sender,
        host.io().unwrap().0,
        Policy::default(),
    )
    .unwrap();
    Hosted {
        streaming: host.into_streaming(stream).unwrap(),
        selected,
        control,
        renewal_during_configuration: renewed,
        n,
    }
}
fn configure_proxy(image: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-observer-configure-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let script = include_str!("../decoder_proxy.py")
        .replace("@IMAGE@", &format!("{:?}", image.to_str().unwrap()))
        .replace("@DELAY@", "0")
        .replace(
            "if kind in (4, 6):",
            "if kind == 8: time.sleep(1.2)\n        if kind in (4, 6):",
        );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}
fn exercise(delayed: bool) {
    let image = worker_image();
    let decoder_image = if delayed {
        configure_proxy(&image)
    } else {
        image.clone()
    };
    run(|c, h| async move {
        let mut source_display = Display::start();
        let mut target_display = Display::start();
        let first_color = 0x0050_3060;
        source_display.paint(first_color);
        let (host, viewer) = startup(&c, &h).await;
        let notice = Arc::new(AtomicUsize::new(0));
        let heard = notice.clone();
        let choices = Arc::new(AtomicUsize::new(0));
        let chosen = choices.clone();
        let attempt = viewer.observe(
            target_display.launch(&decoder_image, Role::Present, 142),
            ObserverPolicy::default(),
            move |catalog| {
                assert_eq!(catalog.displays().len(), 1);
                chosen.fetch_add(1, Ordering::Relaxed);
                Ok(Some(9))
            },
            move |_| {
                heard.fetch_add(1, Ordering::Release);
                Ok(())
            },
        );
        let (observed, hosted) = Box::pin(support::both(
            attempt,
            // Keep the manual host fixture's large decoder continuation off
            // the ordinary test stack, as the public host bootstrap already does.
            Box::pin(host_bootstrap(host, &h, &image, &source_display, &notice)),
        ))
        .await;
        let mut observed = observed.unwrap();
        let Hosted {
            mut streaming,
            selected,
            control,
            renewal_during_configuration,
            mut n,
        } = hosted;
        assert_eq!(notice.load(Ordering::Acquire), 1);
        assert_eq!(choices.load(Ordering::Relaxed), 1);
        assert_eq!(observed.display().handle, 9);
        assert_eq!(observed.initial_presentation().frame.as_raw(), 0);
        assert_eq!(
            observed.initial_presentation().stage,
            media::PresentationStage::SubmittedToCompositor
        );
        target_display.assert_pixel(first_color);
        if delayed {
            assert!(
                renewal_during_configuration,
                "native configure blocked observation renewal"
            );
        }
        let pid = observed.worker_id();
        let source_pid = streaming.worker_id();
        let next_color = 0x0030_6080;
        source_display.paint(next_color);
        let stop = observed.control();
        let until = now(&c).unwrap() + 3_150_000;
        let mut presented = false;
        let (a, b) = Box::pin(support::both(
            streaming.serve(|| nonce(&mut n), || None, block),
            observed.serve(|frame| {
                if let Some(frame) = frame {
                    assert!(frame.frame.as_raw() >= 1);
                    target_display.assert_pixel(next_color);
                    presented = true;
                }
                if now(&c).unwrap() >= until {
                    assert!(control.check().is_ok());
                    stop.stop();
                    control.revoke();
                }
                Ok(())
            }),
        ))
        .await;
        assert!(a.is_err() && b.is_err());
        assert!(presented, "no dependent picture through public observer");
        assert!(
            support::clock(&c) >= until,
            "session closed before requested stop: {a:?} {b:?}"
        );
        assert_eq!(observed.worker_id(), pid);
        assert_eq!(streaming.worker_id(), source_pid);
        assert!(observed.statistics().network_turns > 10);
        observed
            .reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
        streaming
            .reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
        drop(selected);
    });
}
#[test]
#[ignore = "explicit native lane requires FR_NATIVE_TEST_WORKER and Xvfb"]
fn public_observer_runs_approval_selection_configuration_and_real_hevc_without_manual_viewer_stages()
 {
    exercise(false);
}
#[test]
#[ignore = "explicit native lane requires FR_NATIVE_TEST_WORKER and Xvfb"]
fn public_observer_services_renewal_during_delayed_real_decoder_configuration() {
    exercise(true);
}

mod publisher;
