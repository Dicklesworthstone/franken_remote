//! The dispatch Driver's exclusive controlled share, through the PUBLIC cold
//! routing: real UDP/TLS, supervised capture/present PROTOCOL fixtures, and
//! frd's real out-of-process input path (`input_process`) with a Python
//! PROTOCOL fixture standing in for `fr-input-agent`. Media and input effects
//! are fixtures, not hardware evidence; the namespace e2e uses real binaries.
use super::handoff::{fixture_launch, key, keys};
use super::*;
use crate::{
    input_process::tests::{fixture as agent_fixture, transcript},
    session_agent::{
        ApprovalMode, PermissionKind, PermissionStatus, PlatformKind, SessionAgent,
        source::{
            desktop::{ControlProfile, LocalAction, dispatch::Error as DispatchError},
            prepare::Setup,
        },
    },
    session_startup::{Configuration as HostConfiguration, Host, Peer, Viewer},
};
use asupersync::types::Budget;
use fr_client::input::Policy as InputPolicy;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    limits::ProtocolLimits,
};
use fr_media::delivery::SharedFramePool;
use fr_wire::negotiation::ControlBinding;
use std::sync::atomic::AtomicBool;

async fn connection(c: &Cx, h: &Cx, remote: u128, role: Role) -> (Host, Viewer) {
    let configuration = HostConfiguration {
        offer: host_offer(true),
        binding: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(remote),
        },
        require_approval: false,
        startup_timeout: Duration::from_secs(4),
        authority: AuthorityPolicy::plan_defaults(),
        transport: fr_transport::quic::Policy::default(),
    };
    let offer = match role {
        Role::Observe => fr_client::native::observation_offer(),
        Role::RequestControl => fr_client::native::control_offer(),
    };
    let (client, server) = support::native_pair(c, "localhost", fr_transport::quic::ALPN).await;
    let viewer = Viewer::new(
        c.clone(),
        client.unwrap(),
        offer,
        configuration.transport,
        configuration.startup_timeout,
    )
    .unwrap();
    let host = Host::start(
        h.clone(),
        server.unwrap(),
        Peer::Fixture {
            alive: Arc::new(AtomicBool::new(true)),
            until: now(h).unwrap() + 30_000_000,
            control: true,
        },
        configuration,
    )
    .unwrap();
    (host, viewer)
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_controlling_first_viewer_gets_the_exclusive_share_and_its_input_reaches_the_executor() {
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    let source = runtime.request_cx_with_budget(Budget::INFINITE);
    let later = runtime.request_cx_with_budget(Budget::INFINITE);
    let (agent_image, agent_log) = agent_fixture("normal");
    let seat = Seat::default();
    let mut agent = SessionAgent::new(
        ApprovalMode::Unattended,
        PlatformKind::LinuxX11,
        12,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 8192, 8192).unwrap(),
    );
    agent
        .permissions_mut()
        .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Granted);
    let agent = agent.with_control(
        ControlProfile::new(
            &agent_image,
            ":0",
            None,
            seat.clone(),
            keys(),
            30,
            2_000_000,
            Backend::SoftwareExplicit,
        )
        .unwrap(),
    );
    let ticker = Arc::new(AtomicU64::new(700_000));
    let entropy: crate::session_startup::shared_viewers::Entropy =
        Arc::new(move || Ok(u128::from(ticker.fetch_add(1, Ordering::Relaxed))));
    let (incoming, mut driver) = agent
        .native_incoming(
            source.clone(),
            crate::session_startup::shared_viewers::Policy::default(),
            Duration::from_millis(50),
            entropy,
        )
        .unwrap();
    runtime.block_on(Box::pin(async {
        let cleanup = Cx::current().unwrap();
        let (host, viewer) = connection(&c, &h, 13, Role::RequestControl).await;
        let first = incoming
            .serve_host(host, |_, _| panic!("unattended"))
            .unwrap();
        let factories = Arc::new(AtomicU64::new(0));
        let counted = factories.clone();
        let factory_cx = source.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let local_stop = stop.clone();
        let mut running = Box::pin(driver.serve(
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
                let mut authority = SessionAuthority::new(
                    RemoteSessionId::from_raw(99),
                    AuthorityPolicy::plan_defaults(),
                );
                authority.mark_capabilities_checked().map_err(|_| ())?;
                authority
                    .authorize_observation(crate::media::host_now(&factory_cx).map_err(|_| ())?)
                    .map_err(|_| ())?;
                Ok(Setup {
                    control: crate::media::ObservationControl::new(factory_cx, authority)
                        .map_err(|_| ())?,
                    launch: fixture_launch(WorkerRole::Capture, "unchanged"),
                    pool: SharedFramePool::new(ProtocolLimits::ABSOLUTE, 32 * 1024 * 1024, 8)
                        .map_err(|_| ())?,
                })
            },
            |_| panic!("the controlled share takes the peer's display choice"),
            move |_, _| {
                Ok(if local_stop.load(Ordering::Acquire) {
                    LocalAction::Stop
                } else {
                    LocalAction::Continue
                })
            },
        ));
        let receipts = Cell::new(0);
        let busy = Cell::new(false);
        let mut client = Box::pin(async {
            let _first = first;
            let mut observer = viewer
                .observe_for_control(
                    fixture_launch(WorkerRole::Present, "unchanged"),
                    ObserverPolicy::default(),
                    ClockPolicy::default(),
                    |catalog| Ok(Some(catalog.displays()[0].handle)),
                    |_| panic!("unattended"),
                )
                .await
                .unwrap();
            let request = observer.control_request(1, keys()).unwrap();
            let mut shown = None;
            let mut pressed = false;
            let result = observer
                .serve_requesting_control(
                    1,
                    keys(),
                    InputPolicy::default(),
                    |state, frame| {
                        match state {
                            ViewerControlState::Requesting(pending) => {
                                pending
                                    .confirm_mapping(request.parent, request.target.view)
                                    .unwrap();
                                if let Some(p) = pending.presentation()
                                    && shown != Some(p.frame)
                                {
                                    pending.visible(p.frame.as_raw()).unwrap();
                                    shown = Some(p.frame);
                                }
                            }
                            ViewerControlState::Controlled(input) => {
                                if let Some(p) = frame {
                                    input.visible(p.frame.as_raw()).unwrap();
                                }
                                if !pressed {
                                    let _ = input.action(key(true)).unwrap();
                                    pressed = true;
                                }
                                if receipts.get() == 1 && !busy.get() {
                                    busy.set(true);
                                    stop.store(true, Ordering::Release);
                                }
                            }
                            ViewerControlState::Observing => panic!("lost control intent"),
                        }
                        Ok(())
                    },
                    |_| receipts.set(receipts.get() + 1),
                )
                .await;
            // The host's local stop ended the share; the viewer sees its end.
            assert!(result.is_err());
            observer
                .reap_media(
                    &later,
                    Deadline::after(&later, Duration::from_secs(1)).unwrap(),
                )
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        // While the exclusive share runs, a later viewer is refused Busy.
        let refused_later = Cell::new(false);
        let report = std::future::poll_fn(|task| {
            if let Poll::Ready(result) = running.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(client.as_mut().poll(task).is_pending());
            Poll::Pending
        });
        let report = {
            let mut report = Box::pin(report);
            let mut probe = Box::pin(async {
                while receipts.get() == 0 {
                    asupersync::time::sleep(cleanup.now(), Duration::from_millis(2)).await;
                }
                let (extra, _viewer) = connection(&c, &later, 14, Role::Observe).await;
                assert!(matches!(
                    incoming.serve_host(extra, |_, _| Ok(())),
                    Err(DispatchError::Busy)
                ));
                assert!(later.is_cancel_requested());
                refused_later.set(true);
                std::future::pending::<()>().await;
            });
            std::future::poll_fn(|task| {
                if let Poll::Ready(result) = report.as_mut().poll(task) {
                    return Poll::Ready(result);
                }
                let _ = probe.as_mut().poll(task);
                Poll::Pending
            })
            .await
        }
        .unwrap();
        assert!(refused_later.get(), "no later viewer was tried");
        assert_eq!(factories.load(Ordering::SeqCst), 1);
        assert_eq!(receipts.get(), 1);
        assert_eq!(
            (report.viewers.admitted, report.viewers.finished),
            (1, 1),
            "{report:?}"
        );
        drop(client);
        drop(running);
        // The executor saw exactly the press, then its fence, then the
        // release-only cleanup of the held key, then an orderly stop.
        let lines = transcript(&agent_log);
        let native: Vec<_> = lines
            .iter()
            .filter(|l| l.contains("EFFECT"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            native,
            ["EFFECT KEY", "CLEANUP-EFFECT KEY RELEASE"],
            "{lines:?}"
        );
        let fenced = lines.iter().position(|l| l == "FENCE").expect("fenced");
        let released = lines
            .iter()
            .position(|l| l.starts_with("CLEANUP-EFFECT"))
            .unwrap();
        assert!(fenced < released, "{lines:?}");
        assert_eq!(lines.last().map(String::as_str), Some("STOP"));
        assert!(!seat.is_occupied(), "confirmed cleanup releases the Seat");
        let reaped = driver
            .reap(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
        assert!(reaped.is_some());
        // The proven exit released the publisher and with it the viewer's
        // connection; a later reap reports that same exit, never a new child.
        assert!(driver.worker_id().is_none());
        assert_eq!(
            driver
                .reap(
                    &cleanup,
                    Deadline::after(&cleanup, Duration::from_secs(1)).unwrap()
                )
                .await
                .unwrap(),
            reaped
        );
    }));
}
