//! Actual TLS/UDP -> protected-owner lifecycle -> original native capture IPC.
//! Sysfs/nft/LocalAPI, local consent, capture and decode replies are fixtures.
use super::*;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{CodecConfigurationGeneration, RemoteSessionId},
    input::{DesktopPoint, InputBounds},
    limits::ProtocolLimits,
};
use fr_media::{
    delivery::{MediaBudget, ReceivePipeline, ReceivePolicy, SharedFramePool},
    worker::{Backend, Configuration as Codec, Role as WorkerRole},
};
use fr_transport::quic::{self, Disposition, MediaChannel, Messages, Route, StreamRoute};
use fr_wire::{
    attachment::MediaRole,
    decoder,
    display::{Catalog, Select},
    input::{InputDelivery, InputDirection},
    negotiation::Role,
};
use frd::{
    display_selection::SelectedDisplay,
    media_quic::NegotiatedMedia,
    session_agent::{
        ApprovalMode, PermissionKind, PermissionStatus, PlatformKind, SessionAgent,
        source::{
            desktop::{
                LocalAction,
                dispatch::{self, FinishedFirst},
            },
            prepare::Setup,
        },
    },
    worker::{Deadline, Launch, Retirement},
};
use std::fmt::Write;

// Reuse the existing protocol-only viewer fixture without changing its checks.
#[path = "../../src/session_startup/running/shared/tests/selected/hub/incoming/client.rs"]
mod client;
fn now(cx: &Cx) -> Result<u64, frd::media::Error> {
    frd::media::host_now(cx).map(fr_core::time::HostInstant::as_micros)
}
#[allow(clippy::unnecessary_wraps)]
fn block(_: Route, _: &[u8]) -> Result<Disposition, ()> {
    Ok(Disposition::Blocked)
}
fn codec() -> Codec {
    Codec {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
#[allow(clippy::unnecessary_wraps)]
fn choose(catalog: &Catalog) -> Result<(Select, Codec), ()> {
    Ok((
        catalog.selection(catalog.displays()[0].handle).unwrap(),
        codec(),
    ))
}
fn request() -> Request {
    let mut request = fixture::request();
    request.session.offer = fr_client::native::observation_offer();
    // The optional feedback/recovery extensions are not exercised by this viewer fixture.
    request.session.offer.capabilities.retain(|c| c.required);
    request
}
fn driver(handle: &RuntimeHandle) -> dispatch::Driver {
    let mut agent = SessionAgent::new(
        ApprovalMode::PromptAlways,
        PlatformKind::LinuxX11,
        2,
        InputBounds::new(DesktopPoint { x: -320, y: 40 }, 320, 240).unwrap(),
    );
    // Explicit test-only OS capture permission; never inferred from logind or TLS.
    agent
        .permissions_mut()
        .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Granted);
    let n = AtomicU64::new(400_000);
    let entropy = Arc::new(move || Ok(u128::from(n.fetch_add(1, Ordering::Relaxed))));
    agent
        .native_incoming(
            handle.try_request_cx_with_budget(Budget::INFINITE).unwrap(),
            frd::session_startup::shared_viewers::Policy::default(),
            Duration::from_millis(40),
            entropy,
        )
        .unwrap()
        .1
}
fn setup(handle: &RuntimeHandle) -> (Setup, ObservationControl, Retirement) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut payload = String::new();
    for nal in [
        "40010c01ffff01600000030090000003000003003cba0240",
        "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04",
        "4401c0718112",
        "2801ade06702f86753c11ead2f1f6a69",
    ] {
        write!(payload, "{:08x}{nal}", nal.len() / 2).unwrap();
    }
    let path = std::env::temp_dir().join(format!(
        "fr-protected-desktop-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let script = include_str!("../support/local_source_fixture.py")
        .replace("@MODE@", "changing")
        .replace(
            "b\"synthetic-monitor-unit\"",
            &format!("bytes.fromhex('{payload}')"),
        );
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let cx = handle.try_request_cx_with_budget(Budget::INFINITE).unwrap();
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(100),
        AuthorityPolicy::plan_defaults(),
    );
    authority.mark_capabilities_checked().unwrap();
    authority
        .authorize_observation(frd::media::host_now(&cx).unwrap())
        .unwrap();
    let control = ObservationControl::new(cx, authority).unwrap();
    let (launch, retirement) = Launch::new(&path, ":0", None, WorkerRole::Capture, 91)
        .unwrap()
        .retain_cleanup()
        .unwrap();
    (
        Setup {
            control: control.clone(),
            launch,
            pool: SharedFramePool::new(ProtocolLimits::ABSOLUTE, 32 * 1024 * 1024, 8).unwrap(),
        },
        control,
        retirement,
    )
}
async fn select(viewer: &mut ViewerSession) -> SelectedDisplay {
    let mut choice = viewer.select_display(Duration::from_secs(2)).unwrap();
    let mut chosen = false;
    while !choice.is_complete() {
        choice.dispatch(viewer.io().unwrap().0).unwrap();
        if !chosen && let Some(catalog) = choice.catalog(viewer.io().unwrap().0).unwrap() {
            assert_eq!(
                catalog.displays().len(),
                1,
                "never disclose the neighboring monitor"
            );
            assert_eq!(catalog.displays()[0].x, -320);
            let handle = catalog.displays()[0].handle;
            choice.choose(viewer.io().unwrap().0, handle).unwrap();
            chosen = true;
        }
        choice.transmit(viewer.io().unwrap().0).unwrap();
        viewer.drive(Duration::from_millis(1), block).await.unwrap();
    }
    choice.finish(viewer.io().unwrap().0).unwrap()
}
async fn channel(viewer: &mut ViewerSession, c: &Cx, expected: MediaRole) -> MediaChannel {
    let mut channel: Option<MediaChannel> = None;
    let until = now(c).unwrap() + 1_500_000;
    loop {
        assert!(now(c).unwrap() < until, "shared attachment deadline");
        if let Some(channel) = &mut channel {
            channel
                .transmit(viewer.io().unwrap().0, c, || true)
                .unwrap();
            channel
                .dispatch(viewer.io().unwrap().0, c, || true)
                .unwrap();
            if channel
                .finish(viewer.io().unwrap().0, c, || true)
                .unwrap()
                .is_some()
            {
                assert_eq!(channel.descriptor().role, expected);
                break;
            }
        }
        let mut offer = None;
        viewer
            .drive(Duration::from_millis(1), |_, bytes| {
                if channel.is_none()
                    && bytes.get(6..8) == Some(&(fr_wire::Kind::StreamBinding as u16).to_be_bytes())
                {
                    offer = Some(bytes.to_vec());
                    Ok(Disposition::Consumed)
                } else {
                    Ok(Disposition::Blocked)
                }
            })
            .await
            .unwrap();
        if let Some(offer) = offer {
            channel = Some(
                viewer
                    .accept_media_channel(&offer, Duration::from_secs(2))
                    .unwrap(),
            );
        }
    }
    channel.unwrap()
}

#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn protected_observation_drives_real_media_records_then_reaps_original_source() {
    run(async |broker, connection, c, handle| {
        let tools = Tools::new();
        let api = fixture::Api::new();
        let identity = api.identity(&broker).await;
        let mut listener = Server::new(api.client.clone(), identity.clone())
            .bind_linux(
                &broker,
                tools.configuration(),
                native_accept::Configuration::default(),
            )
            .await
            .unwrap();
        let mut driver = driver(&handle);
        let (setup, source, mut retirement) = setup(&handle);
        let approvals = Arc::new(AtomicU64::new(0));
        let notices = approvals.clone();
        let factories = Arc::new(AtomicU64::new(0));
        let started = factories.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let local_stop = stop.clone();
        let observed = approvals.clone();
        let service = driver.serve_on_linux(
            &mut listener,
            &connection,
            request(),
            move || async move {
                assert_eq!(observed.load(Ordering::Acquire), 1);
                started.fetch_add(1, Ordering::AcqRel);
                Ok(setup)
            },
            choose,
            move |_, _| {
                Ok(if local_stop.load(Ordering::Acquire) {
                    LocalAction::Stop
                } else {
                    LocalAction::Continue
                })
            },
            move |approval, _| {
                notices.fetch_add(1, Ordering::AcqRel);
                approval.decide(true).map_err(|_| ())
            },
        );
        let viewer = async {
            let native = fixture::client(&c, address()).await;
            let v = Viewer::new(
                c.clone(),
                native,
                request().session.offer,
                quic::Policy::default(),
                Duration::from_secs(3),
            )
            .unwrap();
            let mut client = Box::pin(client::Client::start(c.clone(), v)).await;
            client.ready().await;
            while client.frames.len() < 3 {
                client.turn().await;
            }
            stop.store(true, Ordering::Release);
            // Keep the actual viewer alive until the original host terminates it.
            while !connection.is_cancel_requested() {
                sleep(c.now(), Duration::from_millis(1)).await;
            }
            client.frames.len()
        };
        let (completion, frames) = Box::pin(network::both(service, viewer)).await;
        let completion = completion.unwrap();
        assert_eq!(completion.first, FinishedFirst::Desktop);
        assert!(completion.desktop.is_ok());
        assert!(frames >= 3);
        assert_eq!(factories.load(Ordering::Acquire), 1);
        assert!(source.check().is_err());
        assert!(connection.is_cancel_requested());
        assert!(identity.status(&broker).is_ok());
        assert!(
            driver
                .reap(
                    &broker,
                    Deadline::after(&broker, Duration::from_secs(2)).unwrap()
                )
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            retirement
                .reap(
                    &broker,
                    Deadline::after(&broker, Duration::from_secs(2)).unwrap()
                )
                .await
                .unwrap()
                .is_some()
        );
        listener.stop(&broker).await.unwrap();
        assert!(UdpSocket::bind(address()).is_ok());
    });
}
#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn refused_membership_never_creates_a_source_and_terminates_the_waiting_driver() {
    run(async |broker, connection, c, handle| {
        let tools = Tools::new();
        let api = fixture::Api::new();
        let identity = api.identity(&broker).await;
        *api.mode.lock().unwrap() = fixture::Mode::OtherUser;
        let mut listener = Server::new(api.client.clone(), identity.clone())
            .bind_linux(
                &broker,
                tools.configuration(),
                native_accept::Configuration::default(),
            )
            .await
            .unwrap();
        let mut driver = driver(&handle);
        let service = driver.serve_on_linux(
            &mut listener,
            &connection,
            request(),
            || async { panic!("source before membership") },
            choose,
            |_, _| Ok(LocalAction::Continue),
            |_, _| panic!("approval before membership"),
        );
        let (completion, native) =
            Box::pin(network::both(service, fixture::client(&c, address()))).await;
        drop(native);
        let completion = completion.unwrap();
        assert_eq!(completion.first, FinishedFirst::Connection);
        assert_eq!(
            completion.connection,
            Err(LinuxError::Host(HostError::Tailnet(
                fr_tailnet::Error::ScopeDenied
            )))
        );
        assert!(completion.desktop.is_err());
        assert!(driver.worker_id().is_none());
        assert!(identity.status(&broker).is_ok());
        listener.stop(&broker).await.unwrap();
        assert!(UdpSocket::bind(address()).is_ok());
    });
}
#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn unpolled_join_retires_both_owners_without_source_or_credential_work() {
    run(async |broker, connection, _, handle| {
        let tools = Tools::new();
        let api = fixture::Api::new();
        let identity = api.identity(&broker).await;
        let mut listener = Server::new(api.client.clone(), identity.clone())
            .bind_linux(
                &broker,
                tools.configuration(),
                native_accept::Configuration::default(),
            )
            .await
            .unwrap();
        let mut driver = driver(&handle);
        let before = api.whois.load(Ordering::Acquire);
        drop(driver.serve_on_linux(
            &mut listener,
            &connection,
            request(),
            || async { panic!("unpolled factory") },
            choose,
            |_, _| panic!("unpolled local"),
            |_, _| panic!("unpolled consent"),
        ));
        assert!(connection.is_cancel_requested());
        assert_eq!(api.whois.load(Ordering::Acquire), before);
        assert!(identity.status(&broker).is_ok());
        assert!(
            driver
                .reap(
                    &broker,
                    Deadline::after(&broker, Duration::from_secs(1)).unwrap()
                )
                .await
                .unwrap()
                .is_none()
        );
        listener.stop(&broker).await.unwrap();
        assert!(UdpSocket::bind(address()).is_ok());
    });
}
#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn caught_local_panic_eagerly_releases_joined_work_even_with_failed_future_retained() {
    run(async |broker, connection, _, handle| {
        let tools = Tools::new();
        let api = fixture::Api::new();
        let identity = api.identity(&broker).await;
        let mut listener = Server::new(api.client.clone(), identity.clone())
            .bind_linux(
                &broker,
                tools.configuration(),
                native_accept::Configuration::default(),
            )
            .await
            .unwrap();
        let mut driver = driver(&handle);
        let mut service = Box::pin(driver.serve_on_linux(
            &mut listener,
            &connection,
            request(),
            || async { panic!("no source") },
            choose,
            |_, _| panic!("intentional local callback panic"),
            |_, _| panic!("no consent"),
        ));
        poll_fn(|task| {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| service
                    .as_mut()
                    .poll(task)))
                .is_err()
            );
            assert!(connection.is_cancel_requested());
            assert!(UdpSocket::bind(address()).is_ok());
            assert_eq!(
                service.as_mut().poll(task),
                Poll::Ready(Err(dispatch::Error::Closed))
            );
            Poll::Ready(())
        })
        .await;
        drop(service);
        listener.stop(&broker).await.unwrap();
        assert!(identity.status(&broker).is_ok());
    });
}
