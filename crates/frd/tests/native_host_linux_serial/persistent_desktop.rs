//! Actual protected-owner -> TLS -> `LocalAPI` -> Host -> native-source -> viewer
//! composition. Firewall/interface, OS consent, HEVC and decode are FIXTURES.
use super::*;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{CodecConfigurationGeneration, RemoteSessionId},
    input::{DesktopPoint, InputBounds},
    limits::ProtocolLimits,
};
use fr_media::{
    delivery::SharedFramePool,
    worker::{Backend, Configuration as Codec, Role as WorkerRole},
};
use fr_transport::quic::{self, Disposition, Route};
use fr_wire::{
    attachment, decoder,
    display::{Catalog, Select},
    negotiation::{Capability, Role},
};
use frd::{
    native_connection::host::desktop::{Connections, End},
    session_agent::{
        ApprovalMode, PermissionKind, PermissionStatus, PlatformKind, SessionAgent,
        source::{
            desktop::{LocalAction, dispatch},
            prepare::Setup,
        },
    },
    session_startup::{Approval, shared_viewers},
    worker::{Deadline, Launch, Retirement},
};

// Share the same already-registered protocol-only viewer; no parallel harness.
use super::desktop::client::Client;

fn now(cx: &Cx) -> Result<u64, ()> {
    cx.checkpoint().map_err(|_| ())?;
    Ok(cx.timer_driver().ok_or(())?.now().as_nanos() / 1000)
}
#[allow(clippy::unnecessary_wraps)]
fn block(_: Route, _: &[u8]) -> Result<Disposition, ()> {
    Ok(Disposition::Blocked)
}
pub(super) fn offer() -> fr_wire::negotiation::Offer {
    let mut offer = fixture::offer();
    offer.capabilities = [
        decoder::CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
        fr_wire::display::CAPABILITY,
    ]
    .into_iter()
    .map(|name| Capability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect();
    offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    offer
}
fn request(n: u64) -> Result<Request, serial::Error> {
    let mut request = fixture::request();
    request.session.offer = offer();
    request.session.binding.remote_session = RemoteSessionId::from_raw(u128::from(n) + 200);
    request.connection_id =
        asupersync::net::quic_core::ConnectionId::new(&(u128::from(n) + 100).to_be_bytes())
            .map_err(|_| serial::Error::Configuration)?;
    Ok(request)
}
fn entropy() -> shared_viewers::Entropy {
    let n = AtomicU64::new(600_000);
    Arc::new(move || Ok(u128::from(n.fetch_add(1, Ordering::Relaxed))))
}
fn driver(cx: Cx) -> dispatch::Driver {
    let mut agent = SessionAgent::new(
        ApprovalMode::PromptAlways,
        PlatformKind::LinuxX11,
        2,
        InputBounds::new(DesktopPoint { x: -320, y: 40 }, 320, 240).unwrap(),
    );
    // Explicit synthetic local screen permission. logind/TLS cannot supply it.
    agent
        .permissions_mut()
        .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Granted);
    agent
        .native_incoming(
            cx,
            shared_viewers::Policy::default(),
            Duration::from_millis(50),
            entropy(),
        )
        .unwrap()
        .1
}
fn choose(catalog: &Catalog) -> Result<(Select, Codec), ()> {
    Ok((
        catalog
            .selection(catalog.displays()[0].handle)
            .map_err(|_| ())?,
        Codec {
            width: 320,
            height: 240,
            fps: 30,
            backend: Backend::SoftwareExplicit,
            bitrate: 2_000_000,
            max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
            generation: CodecConfigurationGeneration::INITIAL,
        },
    ))
}
/// A scripted capture worker speaking the real worker protocol with fixture
/// HEVC; returns the executable and the path it records its PID in.
pub(super) fn source_script(mode: &str) -> (PathBuf, PathBuf) {
    use std::fmt::Write;
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
    let path = fixture::pki().join(format!(
        "desktop-source-{}.py",
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let trace = path.with_extension("trace");
    fs::write(
        &path,
        include_str!("../support/local_source_fixture.py")
            .replace("@MODE@", mode)
            .replace("configured = False", &format!("with open({:?}, 'w') as trace: trace.write(str(os.getpid()))\nconfigured = False", trace.to_str().unwrap()))
            .replace(
                "b\"synthetic-monitor-unit\"",
                &format!("bytes.fromhex('{payload}')"),
            ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    (path, trace)
}
fn setup(cx: Cx, mode: &str) -> (Setup, ObservationControl, Retirement, PathBuf) {
    let (path, trace) = source_script(mode);
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(901),
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
        trace,
    )
}
async fn bound(
    broker: &Cx,
    api: &fixture::Api,
    tools: &Tools,
) -> frd::native_connection::host::LinuxServer {
    Server::new(api.client.clone(), api.identity(broker).await)
        .bind_linux(
            broker,
            tools.configuration(),
            native_accept::Configuration::default(),
        )
        .await
        .unwrap()
}
async fn cleanup(
    server: &mut frd::native_connection::host::LinuxServer,
    driver: &mut dispatch::Driver,
    retirement: &mut Retirement,
    cx: &Cx,
    started: bool,
) {
    let deadline = Deadline::after(cx, Duration::from_secs(1)).unwrap();
    driver.reap(cx, deadline).await.unwrap();
    assert_eq!(
        retirement.reap(cx, deadline).await.unwrap().is_some(),
        started
    );
    server.stop(cx).await.unwrap();
    assert!(UdpSocket::bind(address()).is_ok());
}
type RequestFactory = fn(u64) -> Result<Request, serial::Error>;
type Completed = fn(
    serial::Statistics,
    frd::native_connection::host::desktop::PeerResult,
) -> Result<serial::Action, serial::Error>;
fn callbacks(
    approval: Arc<Mutex<Option<Approval>>>,
) -> Connections<
    RequestFactory,
    impl FnMut(Approval, Role) -> Result<(), ()> + Clone + Send + 'static,
    Completed,
> {
    Connections {
        request,
        approval: move |notice, role| {
            assert_eq!(role, Role::Observe);
            *approval.lock().unwrap() = Some(notice);
            Ok(())
        },
        completed: |_, _| Ok(serial::Action::Continue),
    }
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn protected_listener_reaches_selected_source_and_delivers_a_frame_after_actual_approval() {
    run(async |broker, supervisor, client, runtime| {
        let api = fixture::Api::new();
        let tools = Tools::new();
        let mut server = bound(&broker, &api, &tools).await;
        let source = runtime
            .try_request_cx_with_budget(Budget::INFINITE)
            .unwrap();
        let (setup, control, mut retirement, trace) = setup(source.clone(), "normal");
        let mut driver = driver(source);
        let notices = Arc::new(Mutex::new(None));
        let factories = Arc::new(AtomicU64::new(0));
        let count = factories.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let local_stop = stop.clone();
        let serving = server.serve_desktop(
            &mut driver,
            supervisor.clone(),
            runtime,
            serial::Policy::default(),
            callbacks(notices.clone()),
            move || async move {
                count.fetch_add(1, Ordering::Relaxed);
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
        );
        let viewing = async {
            let native = fixture::client(&client, address()).await;
            let mut viewer = Viewer::new(
                client.clone(),
                native,
                offer(),
                quic::Policy::default(),
                Duration::from_secs(3),
            )
            .unwrap();
            while notices.lock().unwrap().is_none() {
                viewer.drive(Duration::from_millis(1)).await.unwrap();
            }
            assert_eq!(
                factories.load(Ordering::Acquire),
                0,
                "no source before local decision"
            );
            assert!(!trace.exists(), "no native child before consent");
            notices
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .decide(true)
                .unwrap();
            let mut viewer = Box::pin(Client::start(client.clone(), viewer)).await;
            viewer.ready().await;
            assert_eq!(viewer.frames.first(), Some(&0));
            let until = now(&client).unwrap() + 3_100_000;
            while now(&client).unwrap() < until {
                viewer.turn().await;
            }
            assert!(
                control.check().is_ok(),
                "source renewal outlives initial lease"
            );
            assert_eq!(factories.load(Ordering::Acquire), 1);
            stop.store(true, Ordering::Release);
        };
        let (end, ()) = Box::pin(network::both(serving, viewing)).await;
        assert!(matches!(end, End::Desktop(Ok(_))), "{end:?}");
        assert!(supervisor.is_cancel_requested());
        assert!(!broker.is_cancel_requested());
        assert!(control.check().is_err());
        cleanup(&mut server, &mut driver, &mut retirement, &broker, true).await;
        assert_eq!(tools.state()["created"], 1);
        assert_eq!(tools.state()["deleted"], 1);
    });
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn cancelling_idle_composition_never_starts_factory_and_retires_original_socket() {
    idle(false);
}
#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn dropping_unpolled_composition_closes_unused_listener_and_cold_dispatcher() {
    idle(true);
}
fn idle(unpolled: bool) {
    run(async move |broker, supervisor, _, runtime| {
        let api = fixture::Api::new();
        let tools = Tools::new();
        let mut server = bound(&broker, &api, &tools).await;
        let source = runtime
            .try_request_cx_with_budget(Budget::INFINITE)
            .unwrap();
        let mut driver = driver(source);
        let incoming = driver.incoming();
        let requests = Arc::new(AtomicU64::new(0));
        let observed = requests.clone();
        let connections = callbacks(Arc::new(Mutex::new(None)));
        let serving = server.serve_desktop(
            &mut driver,
            supervisor.clone(),
            runtime,
            serial::Policy::default(),
            Connections {
                request: move |n| {
                    observed.fetch_add(1, Ordering::Relaxed);
                    (connections.request)(n)
                },
                approval: connections.approval,
                completed: connections.completed,
            },
            || async { panic!("idle source factory") },
            choose,
            |_, _| Ok(LocalAction::Continue),
        );
        if unpolled {
            drop(serving);
            assert_eq!(requests.load(Ordering::Acquire), 0);
        } else {
            let cancel = async {
                until(&broker, || requests.load(Ordering::Acquire) > 0).await;
                supervisor.cancel_fast(CancelKind::User);
            };
            let (end, ()) = Box::pin(network::both(serving, cancel)).await;
            assert_eq!(end, End::Cancelled);
        }
        assert!(supervisor.is_cancel_requested());
        assert!(!broker.is_cancel_requested());
        assert!(
            UdpSocket::bind(address()).is_ok(),
            "unpolled original listener must drop"
        );
        incoming.close();
        server.stop(&broker).await.unwrap();
        assert_eq!(tools.state()["deleted"], 1);
    });
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn denied_observer_cannot_start_source_through_composed_host() {
    run(async |broker, supervisor, client, runtime| {
        let api = fixture::Api::new();
        let tools = Tools::new();
        let mut server = bound(&broker, &api, &tools).await;
        let source = runtime
            .try_request_cx_with_budget(Budget::INFINITE)
            .unwrap();
        let mut driver = driver(source);
        let notices = Arc::new(Mutex::new(None));
        let serving = server.serve_desktop(
            &mut driver,
            supervisor.clone(),
            runtime,
            serial::Policy::default(),
            callbacks(notices.clone()),
            || async { panic!("source before consent") },
            choose,
            |_, _| Ok(LocalAction::Continue),
        );
        let viewing = async {
            let native = fixture::client(&client, address()).await;
            let mut viewer = Viewer::new(
                client.clone(),
                native,
                offer(),
                quic::Policy::default(),
                Duration::from_secs(3),
            )
            .unwrap();
            while notices.lock().unwrap().is_none() {
                viewer.drive(Duration::from_millis(1)).await.unwrap();
            }
            notices
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .decide(false)
                .unwrap();
            while viewer.drive(Duration::from_millis(1)).await.is_ok() {
                assert!(!viewer.is_complete(), "denied observer cannot open");
            }
        };
        let (end, ()) = Box::pin(network::both(serving, viewing)).await;
        assert!(matches!(end, End::Desktop(Err(_))), "{end:?}");
        assert!(supervisor.is_cancel_requested());
        assert!(!broker.is_cancel_requested());
        assert_eq!(driver.worker_id(), None);
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
        assert!(UdpSocket::bind(address()).is_ok());
        server.stop(&broker).await.unwrap();
    });
}

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn ingress_loss_drops_original_pending_factory_even_with_terminal_future_retained() {
    run(async |broker, supervisor, client, runtime| {
        let api = fixture::Api::new();
        let tools = Tools::new();
        let mut server = bound(&broker, &api, &tools).await;
        let source = runtime
            .try_request_cx_with_budget(Budget::INFINITE)
            .unwrap();
        let mut driver = driver(source);
        let notices = Arc::new(Mutex::new(None));
        let entered = Arc::new(AtomicBool::new(false));
        let called = entered.clone();
        let dropped = Arc::new(AtomicBool::new(false));
        let marker = Dropped(dropped.clone());
        let mut serving = Box::pin(server.serve_desktop(
            &mut driver,
            supervisor.clone(),
            runtime,
            serial::Policy::default(),
            callbacks(notices.clone()),
            move || async move {
                let _marker = marker;
                called.store(true, Ordering::Release);
                pending::<Result<Setup, ()>>().await
            },
            choose,
            |_, _| Ok(LocalAction::Continue),
        ));
        let viewing = async {
            let native = fixture::client(&client, address()).await;
            let mut viewer = Viewer::new(
                client.clone(),
                native,
                offer(),
                quic::Policy::default(),
                Duration::from_secs(3),
            )
            .unwrap();
            while notices.lock().unwrap().is_none() {
                viewer.drive(Duration::from_millis(1)).await.unwrap();
            }
            notices
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .decide(true)
                .unwrap();
            while !viewer.is_complete() {
                viewer.drive(Duration::from_millis(1)).await.unwrap();
            }
            let mut viewer = viewer.finish().unwrap();
            while !entered.load(Ordering::Acquire) {
                viewer.drive(Duration::from_millis(1), block).await.unwrap();
            }
            fs::write(tools.0.join("tamper"), b"synthetic rule removal").unwrap();
            while !supervisor.is_cancel_requested() {
                if viewer.drive(Duration::from_millis(1), block).await.is_err() {
                    break;
                }
            }
        };
        let (end, ()) = Box::pin(network::both(serving.as_mut(), viewing)).await;
        assert!(
            matches!(
                end,
                End::Listener(Err(LinuxError::Ingress(ingress::Error::FirewallMismatch)))
            ),
            "{end:?}"
        );
        assert!(
            dropped.load(Ordering::Acquire),
            "do not retain abandoned factory work"
        );
        assert!(supervisor.is_cancel_requested());
        assert!(!broker.is_cancel_requested());
        assert!(
            UdpSocket::bind(address()).is_ok(),
            "completed future must not retain socket"
        );
        assert_eq!(
            serving
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop())),
            Poll::Ready(end)
        );
        drop(serving);
        assert_eq!(driver.worker_id(), None);
        server.stop(&broker).await.unwrap();
    });
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn caught_local_panic_after_first_frame_fences_source_and_original_listener_before_drop() {
    run(async |broker, supervisor, client, runtime| {
        let api = fixture::Api::new();
        let tools = Tools::new();
        let mut server = bound(&broker, &api, &tools).await;
        let source = runtime
            .try_request_cx_with_budget(Budget::INFINITE)
            .unwrap();
        let (setup, control, mut retirement, _) = setup(source.clone(), "normal");
        let mut driver = driver(source);
        let notices = Arc::new(Mutex::new(None));
        let panicking = Arc::new(AtomicBool::new(false));
        let local = panicking.clone();
        let mut serving = Box::pin(server.serve_desktop(
            &mut driver,
            supervisor.clone(),
            runtime,
            serial::Policy::default(),
            callbacks(notices.clone()),
            move || async move { Ok(setup) },
            choose,
            move |_, _| {
                assert!(!local.load(Ordering::Acquire), "local event fixture panic");
                Ok(LocalAction::Continue)
            },
        ));
        let viewing = async {
            let native = fixture::client(&client, address()).await;
            let mut viewer = Viewer::new(
                client.clone(),
                native,
                offer(),
                quic::Policy::default(),
                Duration::from_secs(3),
            )
            .unwrap();
            while notices.lock().unwrap().is_none() {
                viewer.drive(Duration::from_millis(1)).await.unwrap();
            }
            notices
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .decide(true)
                .unwrap();
            let mut viewer = Box::pin(Client::start(client.clone(), viewer)).await;
            viewer.ready().await;
            assert_eq!(viewer.frames.first(), Some(&0));
            panicking.store(true, Ordering::Release);
        };
        let caught = poll_fn(|task| {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                serving.as_mut().poll(task)
            })) {
                Ok(Poll::Pending) => Poll::Pending,
                Ok(Poll::Ready(end)) => panic!("unexpected clean completion: {end:?}"),
                Err(_) => Poll::Ready(()),
            }
        });
        Box::pin(network::both(caught, viewing)).await;
        assert!(control.check().is_err());
        assert!(supervisor.is_cancel_requested());
        assert!(!broker.is_cancel_requested());
        assert!(UdpSocket::bind(address()).is_ok());
        assert_eq!(
            serving
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop())),
            Poll::Ready(End::Cancelled)
        );
        drop(serving);
        cleanup(&mut server, &mut driver, &mut retirement, &broker, true).await;
    });
}

// One explicit synthetic local decision on the real one-use approval object.
async fn approve(client: &Cx, notices: &Mutex<Option<Approval>>) -> Viewer {
    let native = fixture::client(client, address()).await;
    let mut viewer = Viewer::new(
        client.clone(),
        native,
        offer(),
        quic::Policy::default(),
        Duration::from_secs(3),
    )
    .unwrap();
    while notices.lock().unwrap().is_none() {
        viewer.drive(Duration::from_millis(1)).await.unwrap();
    }
    notices
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .decide(true)
        .unwrap();
    viewer
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn peer_refusal_keeps_cold_desktop_available_for_next_authorized_client() {
    run(async |broker, supervisor, client, runtime| {
        let api = fixture::Api::new();
        let tools = Tools::new();
        let mut server = bound(&broker, &api, &tools).await;
        *api.mode.lock().unwrap() = fixture::Mode::OtherUser;
        let source = runtime
            .try_request_cx_with_budget(Budget::INFINITE)
            .unwrap();
        let created = Arc::new(Mutex::new(None));
        let saved = created.clone();
        let mut driver = driver(source.clone());
        let notices = Arc::new(Mutex::new(None));
        let refused = Arc::new(AtomicBool::new(false));
        let recorded = refused.clone();
        let finished = Arc::new(AtomicBool::new(false));
        let completed = finished.clone();
        let requests = Arc::new(AtomicU64::new(0));
        let counted = requests.clone();
        let factories = Arc::new(AtomicU64::new(0));
        let made = factories.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let local = stop.clone();
        let connections = callbacks(notices.clone());
        let serving = server.serve_desktop(
            &mut driver,
            supervisor.clone(),
            runtime.clone(),
            serial::Policy::default(),
            Connections {
                request: move |n| {
                    counted.store(n, Ordering::Release);
                    request(n)
                },
                approval: connections.approval,
                completed: move |stats: serial::Statistics, outcome: frd::native_connection::host::desktop::PeerResult| {
                    if stats.attempts == 1 {
                        assert_eq!(outcome, Err(HostError::Tailnet(fr_tailnet::Error::ScopeDenied)));
                        assert!(!recorded.swap(true, Ordering::AcqRel));
                    } else {
                        assert_eq!(stats.attempts, 2);
                        assert!(recorded.load(Ordering::Acquire));
                        assert_eq!(outcome, Err(HostError::Cancelled));
                        assert!(!completed.swap(true, Ordering::AcqRel));
                    }
                    Ok(serial::Action::Continue)
                },
            },
            move || async move {
                // Authority is created after the authorized peer consents, not
                // left aging across the previous refused connection.
                let (setup, control, retirement, trace) = setup(source, "normal");
                *saved.lock().unwrap() = Some((control, retirement, trace));
                made.fetch_add(1, Ordering::AcqRel);
                Ok(setup)
            },
            choose,
            move |_, _| {
                Ok(if local.load(Ordering::Acquire) {
                    LocalAction::Stop
                } else {
                    LocalAction::Continue
                })
            },
        );
        let viewing = async {
            let first = fixture::client(&client, address()).await;
            until(&broker, || refused.load(Ordering::Acquire)).await;
            assert_eq!(factories.load(Ordering::Acquire), 0);
            assert!(
                created.lock().unwrap().is_none(),
                "no source authority or native launch for refused peer"
            );
            assert!(notices.lock().unwrap().is_none());
            drop(first);
            *api.mode.lock().unwrap() = fixture::Mode::Allowed;
            until(&broker, || requests.load(Ordering::Acquire) == 2).await;
            let next = runtime
                .try_request_cx_with_budget(Budget::INFINITE)
                .unwrap();
            let viewer = approve(&next, &notices).await;
            let mut viewer = Box::pin(Client::start(next, viewer)).await;
            viewer.ready().await;
            assert_eq!(viewer.frames.first(), Some(&0));
            assert_eq!(factories.load(Ordering::Acquire), 1);
            stop.store(true, Ordering::Release);
        };
        let (end, ()) = Box::pin(network::both(serving, viewing)).await;
        assert!(matches!(end, End::Desktop(Ok(_))), "{end:?}");
        assert!(supervisor.is_cancel_requested());
        assert!(!broker.is_cancel_requested());
        assert!(
            finished.load(Ordering::Acquire),
            "report the admitted peer too"
        );
        assert_eq!(requests.load(Ordering::Acquire), 2);
        let (control, mut retirement, _) = created.lock().unwrap().take().unwrap();
        assert!(control.check().is_err());
        cleanup(&mut server, &mut driver, &mut retirement, &broker, true).await;
        assert_eq!(tools.state()["created"], 1);
    });
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn departed_cold_observer_reports_completion_without_accepting_another_peer() {
    run(async |broker, supervisor, client, runtime| {
        let api = fixture::Api::new();
        let tools = Tools::new();
        let mut server = bound(&broker, &api, &tools).await;
        let source = runtime
            .try_request_cx_with_budget(Budget::INFINITE)
            .unwrap();
        let (setup, control, mut retirement, trace) = setup(source.clone(), "normal");
        let mut driver = driver(source);
        let requests = Arc::new(AtomicU64::new(0));
        let requested = requests.clone();
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let completed = outcomes.clone();
        let serving = server.serve_desktop(
            &mut driver,
            supervisor.clone(),
            runtime,
            serial::Policy::default(),
            Connections {
                request: move |n| {
                    requested.fetch_add(1, Ordering::Relaxed);
                    request(n)
                },
                approval: |notice: Approval, _| notice.decide(true).map_err(|_| ()),
                completed: move |stats, result| {
                    completed.lock().unwrap().push((stats, result));
                    // A terminal desktop must not act on this Continue by
                    // admitting another peer into its already-closed source.
                    Ok(serial::Action::Continue)
                },
            },
            move || async move { Ok(setup) },
            choose,
            |_, _| Ok(LocalAction::Continue),
        );
        let departing = async {
            let native = fixture::client(&client, address()).await;
            let mut viewer = Viewer::new(
                client.clone(),
                native,
                offer(),
                quic::Policy::default(),
                Duration::from_secs(3),
            )
            .unwrap();
            while !viewer.is_complete() {
                viewer.drive(Duration::from_millis(5)).await.unwrap();
            }
            let mut session = viewer.finish().unwrap();
            while !trace.exists() {
                session
                    .drive(Duration::from_millis(5), block)
                    .await
                    .unwrap();
            }
            // Leave after real source launch but before selecting a display.
            // No fabricated close/renewal and no deadline extension: the host
            // must observe this peer's original bounded failure itself.
            session.close();
        };
        let (end, ()) = Box::pin(network::both(serving, departing)).await;
        assert!(matches!(end, End::Desktop(Err(_))), "{end:?}");
        {
            let results = outcomes.lock().unwrap();
            assert_eq!(results.len(), 1, "real peer completion lost: {end:?}");
            assert!(results[0].1.is_err() || matches!(results[0].1, Ok(Err(_))));
            assert_eq!(results[0].0.attempts, 1);
        }
        assert_eq!(requests.load(Ordering::Acquire), 1);
        assert!(supervisor.is_cancel_requested());
        assert!(!broker.is_cancel_requested());
        assert!(control.check().is_err());
        cleanup(&mut server, &mut driver, &mut retirement, &broker, true).await;
        assert_eq!(tools.state()["deleted"], 1);
    });
}
