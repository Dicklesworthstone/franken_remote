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
    connection_with(c, h, remote, role, false).await
}
/// With `clipboard`, both sides also offer the optional clipboard boundaries.
async fn connection_with(
    c: &Cx,
    h: &Cx,
    remote: u128,
    role: Role,
    clipboard: bool,
) -> (Host, Viewer) {
    let configuration = HostConfiguration {
        offer: host_offer_with(true, clipboard),
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
        Role::RequestControl if clipboard => fr_client::native::control_offer_with_clipboard(),
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

/// One installed agent image with both roles: `--clipboard` selects the
/// clipboard PROTOCOL fixture, anything else the input one.
fn agent_with_clipboard(input: &std::path::Path, clipboard: &std::path::Path) -> PathBuf {
    let image = input.with_extension("both.py");
    let source = input.with_extension("both.src");
    std::fs::write(
        &source,
        format!(
            "#!/usr/bin/python3\nimport os, sys\nimage = '{}' if sys.argv[1:2] == ['--clipboard'] else '{}'\nos.execv(image, [image] + sys.argv[1:])\n",
            clipboard.display(),
            input.display()
        ),
    )
    .unwrap();
    // Written by another process: no writable descriptor here (ETXTBSY).
    assert!(
        std::process::Command::new("cp")
            .arg(&source)
            .arg(&image)
            .status()
            .unwrap()
            .success()
    );
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o700)).unwrap();
    image
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_clipboard_profile_follows_the_lease_through_its_own_child_both_ways() {
    use super::clipboard::{Os, configured, copy};
    use crate::clipboard_process::tests::{
        fixture as clipboard_fixture, local_copy, transcript as clipboard_transcript,
    };
    use crate::native_clipboard::{Cleanup, Phase};
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    let source = runtime.request_cx_with_budget(Budget::INFINITE);
    let later = runtime.request_cx_with_budget(Budget::INFINITE);
    let (input_image, input_log) = agent_fixture("normal");
    let (clipboard_image, clipboard_log) = clipboard_fixture("normal");
    let image = agent_with_clipboard(&input_image, &clipboard_image);
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
            &image,
            ":0",
            None,
            seat.clone(),
            keys(),
            30,
            2_000_000,
            Backend::SoftwareExplicit,
        )
        .unwrap()
        .with_clipboard(),
    );
    let ticker = Arc::new(AtomicU64::new(900_000));
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
    let os = Arc::new(Mutex::new(Os::default()));
    let stage = Cell::new(0);
    let viewer_copy = "viewer λ copy";
    let host_copy = "host ✓ copy";
    runtime.block_on(Box::pin(async {
        let cleanup = Cx::current().unwrap();
        let (host, viewer) = connection_with(&c, &h, 13, Role::RequestControl, true).await;
        let first = incoming
            .serve_host(host, |_, _| panic!("unattended"))
            .unwrap();
        let factory_cx = source.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let local_stop = stop.clone();
        let mut running = Box::pin(driver.serve(
            move || {
                let mut authority = SessionAuthority::new(
                    RemoteSessionId::from_raw(98),
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
        let start = now(&c).unwrap();
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
            let remote = observer
                .configure_clipboard(configured(&os, true, None))
                .unwrap();
            let request = observer.control_request(1, keys()).unwrap();
            let mut shown = None;
            let result = observer
                .serve_requesting_control(
                    1,
                    keys(),
                    InputPolicy::default(),
                    |state, frame| {
                        assert!(
                            now(&c).unwrap() < start + 20_000_000,
                            "stalled at stage {}: {:?} {:?}",
                            stage.get(),
                            remote.status(),
                            clipboard_transcript(&clipboard_log)
                        );
                        match state {
                            ViewerControlState::Requesting(pending) => {
                                // No lease yet: no clipboard child, no native
                                // clipboard opened on either side.
                                assert_eq!(
                                    clipboard_transcript(&clipboard_log),
                                    Vec::<String>::new()
                                );
                                assert!(!os.lock().unwrap().opened);
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
                                let published = format!("PUBLISH {}", viewer_copy.len());
                                match stage.get() {
                                    // Viewer copy -> host child publication.
                                    0 if remote.status().phase == Phase::Running => {
                                        copy(&os, viewer_copy);
                                        stage.set(1);
                                    }
                                    1 if clipboard_transcript(&clipboard_log)
                                        .contains(&published) =>
                                    {
                                        // Host copy -> viewer publication.
                                        local_copy(&clipboard_log, host_copy);
                                        stage.set(2);
                                    }
                                    2 if os
                                        .lock()
                                        .unwrap()
                                        .published
                                        .iter()
                                        .any(|t| t == host_copy) =>
                                    {
                                        stage.set(3);
                                        stop.store(true, Ordering::Release);
                                    }
                                    _ => {}
                                }
                                // Drain the viewer's two receipt slots.
                                while remote.take_received().is_some() {}
                            }
                            ViewerControlState::Observing => panic!("lost control intent"),
                        }
                        Ok(())
                    },
                    |_| {},
                )
                .await;
            // The host's local stop ended the share; the viewer sees its end.
            assert!(result.is_err());
            let deadline = Deadline::after(&later, Duration::from_secs(1)).unwrap();
            assert!(matches!(
                observer.reap_clipboard(&later, deadline).await.unwrap(),
                Cleanup::Finished(_)
            ));
            observer.reap_media(&later, deadline).await.unwrap();
            std::future::pending::<()>().await;
        });
        let report = std::future::poll_fn(|task| {
            if let Poll::Ready(result) = running.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(client.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await
        .unwrap();
        assert_eq!(stage.get(), 3, "both directions crossed");
        assert_eq!((report.viewers.admitted, report.viewers.finished), (1, 1));
        drop(client);
        drop(running);
        // The host's clipboard child: launched only after the grant, published
        // exactly the viewer's item, served the host copy, then stopped in order.
        let lines = clipboard_transcript(&clipboard_log);
        assert!(lines[0].starts_with("START "), "{lines:?}");
        assert_eq!(lines[1], "HELLO 1048576", "{lines:?}");
        assert!(
            lines.contains(&format!("PUBLISH {}", viewer_copy.len())),
            "{lines:?}"
        );
        assert!(
            lines.contains(&format!("READ {}", host_copy.len())),
            "{lines:?}"
        );
        assert_eq!(lines.last().map(String::as_str), Some("STOP"), "{lines:?}");
        // The lease's input executor ran and stopped in its own child.
        let input = transcript(&input_log);
        assert_eq!(input.last().map(String::as_str), Some("STOP"), "{input:?}");
        let reaped = driver
            .reap(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(2)).unwrap(),
            )
            .await
            .unwrap();
        assert!(reaped.is_some());
        let pid = lines[0].split(' ').nth(1).unwrap();
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "clipboard child reaped"
        );
        assert!(!seat.is_occupied());
        let os = os.lock().unwrap();
        assert!(
            os.opened && os.closed,
            "viewer native owner opened and closed"
        );
    }));
}
