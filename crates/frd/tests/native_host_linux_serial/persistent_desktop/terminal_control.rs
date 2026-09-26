//! Global supervisor cancellation through the actual protected listener,
//! exclusive desktop dispatcher, managed input child and controlled viewer.
//! The LocalAPI/ingress, capture/decode completions and native key effects are
//! explicit protocol fixtures, not live-tailnet, `XTest` or physical-device proof.
use super::*;
use fr_client::input::{Action, Policy as InputPolicy};
use fr_core::{
    input::{KeyTransition, PhysicalKey},
    input_submission::{Capabilities, Capability as InputCapability},
};
use fr_media::freshness::ClockPolicy;
use fr_wire::lease_revoked::{CleanupStage, EffectStage};
use frd::{
    input_agent::Seat,
    session_agent::source::desktop::ControlProfile,
    session_startup::{
        ControlledViewerError, ObserverError, ObserverPolicy, StreamingViewerError,
        ViewerControlState,
    },
};
use std::cell::Cell;

fn executable(name: &str, body: &str) -> PathBuf {
    let path = fixture::pki().join(name);
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}
fn input_fixture() -> (PathBuf, PathBuf) {
    let log = fixture::pki().join("terminal-input.log");
    let image = executable(
        "terminal-input.py",
        &include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/input_process/agent_fixture.py"
        ))
        .replace("@MODE@", "normal")
        .replace("@LOG@", log.to_str().unwrap()),
    );
    (image, log)
}
fn presenter_fixture() -> Launch {
    let image = executable(
        "terminal-presenter.py",
        &include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/session_startup/native_control/tests/handoff_worker_fixture.py"
        ))
        .replace("@MODE@", "unchanged")
        .replace("@CATALOG@", ""),
    );
    Launch::new(&image, ":0", None, WorkerRole::Present, 701).unwrap()
}
fn keys() -> Capabilities {
    Capabilities::default().with(InputCapability::Keys)
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress and native-effect fixtures"]
#[allow(clippy::too_many_lines)]
fn supervisor_stop_delivers_revocation_then_reaps_the_original_controlled_desktop() {
    run(async |broker, supervisor, client, runtime| {
        let api = fixture::Api::new();
        let tools = Tools::new();
        let mut server = bound(&broker, &api, &tools).await;
        let source = runtime
            .try_request_cx_with_budget(Budget::INFINITE)
            .unwrap();
        let (setup, _unused_source_control, mut retirement, _) = setup(source.clone(), "normal");
        let (image, log) = input_fixture();
        let seat = Seat::default();
        let mut agent = SessionAgent::new(
            ApprovalMode::Unattended,
            PlatformKind::LinuxX11,
            2,
            InputBounds::new(DesktopPoint { x: -320, y: 40 }, 320, 240).unwrap(),
        );
        agent
            .permissions_mut()
            .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Granted);
        let (_, mut driver) = agent
            .with_control(
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
                .unwrap(),
            )
            .native_incoming(
                source.clone(),
                shared_viewers::Policy::default(),
                // A genuine quiet interval in this fixture allows ACKs to drain.
                // Native backlog still refuses reporting; no buffers are erased
                // and no record lifetime, lease or transport limit is widened.
                Duration::from_millis(200),
                entropy(),
            )
            .unwrap();
        let connections = Connections {
            request: |n| {
                let mut request = request(n)?;
                request.session.offer = frd::session_startup::host_offer(true);
                request.session.require_approval = false;
                Ok(request)
            },
            approval: |_, _| panic!("explicit unattended profile"),
            completed: |_, _| Ok(serial::Action::Continue),
        };
        let serving = server.serve_desktop(
            &mut driver,
            supervisor.clone(),
            runtime,
            serial::Policy::default(),
            connections,
            move || async move { Ok(setup) },
            |_| panic!("controlled viewer chooses its display"),
            |_, _| Ok(LocalAction::Continue),
        );
        let receipts = Cell::new(0_u32);
        let stopped = Cell::new(false);
        let viewing = async {
            let native = fixture::client(&client, address()).await;
            let viewer = Viewer::new(
                client.clone(),
                native,
                fr_client::native::control_offer(),
                quic::Policy::default(),
                Duration::from_secs(3),
            )
            .unwrap();
            let mut observer = viewer
                .observe_for_control(
                    presenter_fixture(),
                    ObserverPolicy::default(),
                    ClockPolicy::default(),
                    |catalog| Ok(Some(catalog.displays()[0].handle)),
                    |_| panic!("explicit unattended profile"),
                )
                .await
                .unwrap();
            let request = observer.control_request(1, keys()).unwrap();
            let mut pressed_at = None;
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
                                if let Some(frame) = pending.presentation() {
                                    let _ = pending.visible(frame.frame.as_raw());
                                }
                            }
                            ViewerControlState::Controlled(input) => {
                                if let Some(frame) = frame {
                                    let _ = input.visible(frame.frame.as_raw());
                                }
                                let current = now(&client).unwrap();
                                if pressed_at.is_none() {
                                    let _ = input
                                        .action(Action::Key {
                                            key: PhysicalKey::new(4).unwrap(),
                                            transition: KeyTransition::Press,
                                        })
                                        .unwrap();
                                    pressed_at = Some(current);
                                }
                                if receipts.get() == 1
                                    && current >= pressed_at.unwrap() + 85_000
                                    && !stopped.replace(true)
                                {
                                    // Cancel the OUTER listener, not the viewer or
                                    // a hand-selected host fixture control handle.
                                    supervisor.cancel_fast(CancelKind::User);
                                }
                            }
                            ViewerControlState::Observing => panic!("lost control intent"),
                        }
                        Ok(())
                    },
                    |_| receipts.set(receipts.get() + 1),
                )
                .await;
            let Err(ObserverError::Streaming(StreamingViewerError::Control(
                ControlledViewerError::LeaseRevoked(report),
            ))) = result
            else {
                panic!(
                    "global shutdown lost its actual wire report: {result:?}; stopped={}, receipts={}, source_cancelled={}",
                    stopped.get(),
                    receipts.get(),
                    source.is_cancel_requested()
                );
            };
            assert_ne!(report.lease.as_raw(), 0);
            assert_eq!(report.cleanup, CleanupStage::Fenced);
            assert_eq!(report.effects, EffectStage::Unknown);
            observer
                .reap_media(
                    &broker,
                    Deadline::after(&broker, Duration::from_secs(1)).unwrap(),
                )
                .await
                .unwrap();
        };
        let (end, ()) = Box::pin(network::both(serving, viewing)).await;
        assert_eq!(end, End::Cancelled);
        assert!(stopped.get());
        assert_eq!(
            receipts.get(),
            1,
            "release cleanup is not a new action receipt"
        );
        assert!(
            !seat.is_occupied(),
            "only the original native owner releases its Seat"
        );
        assert!(
            !source.is_cancel_requested(),
            "cleanup does not resurrect a session Cx"
        );
        let transcript = fs::read_to_string(log).unwrap();
        let lines: Vec<_> = transcript.lines().collect();
        assert_eq!(lines.iter().filter(|&&s| s == "EFFECT KEY").count(), 1);
        let fenced = lines.iter().position(|&s| s == "FENCE").unwrap();
        let released = lines
            .iter()
            .position(|&s| s == "CLEANUP-EFFECT KEY RELEASE")
            .unwrap();
        assert!(fenced < released, "{transcript}");
        assert_eq!(lines.last().copied(), Some("STOP"));
        cleanup(&mut server, &mut driver, &mut retirement, &broker, true).await;
    });
}
