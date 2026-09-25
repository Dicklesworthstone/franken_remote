//! Regression coverage of the public pre-negotiation entrypoint and one-use handoff.
//! UDP/TLS and child supervision are real; consent, display and codec are fixtures.
use super::*;
use fr_client::input::{Action, Policy as InputPolicy};
use fr_core::limits::ProtocolLimits;
use fr_wire::display::Catalog;
use fr_wire::negotiation::ControlBinding;
use std::sync::atomic::AtomicUsize;

#[test]
fn native_bootstrap_handles_do_not_copy_inline_receiver_storage() {
    assert!(std::mem::size_of::<session_startup::NativePublisher>() < 4096);
    assert!(std::mem::size_of::<session_startup::NativeObserver>() < 4096);
}

#[test]
#[allow(clippy::too_many_lines)]
fn pre_negotiation_viewer_entrypoint_preserves_distinct_observation_approval() {
    use crate::session_startup::{Configuration as HostConfiguration, Host, Peer, Viewer};
    use fr_core::authority::AuthorityPolicy;
    use fr_wire::negotiation::Offer;
    use std::sync::atomic::AtomicBool;
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let configuration = HostConfiguration {
            offer: Offer {
                versions: vec![0],
                profile: 1,
                profile_version: 0,
                role: Role::RequestControl,
                limits: ProtocolLimits::ABSOLUTE,
                capabilities: capabilities(),
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
        let (client, server) =
            support::native_pair(&c, "localhost", fr_transport::quic::ALPN).await;
        let viewer = Viewer::new(
            c.clone(),
            client.unwrap(),
            configuration.offer.clone(),
            configuration.transport,
            configuration.startup_timeout,
        )
        .unwrap();
        let host = Host::start(
            h.clone(),
            server.unwrap(),
            Peer::Fixture {
                alive: Arc::new(AtomicBool::new(true)),
                until: now(&h).unwrap() + 30_000_000,
                control: true,
            },
            configuration,
        )
        .unwrap();
        let notice_count = AtomicUsize::new(0);
        let local_consent = AtomicBool::new(false);
        let pending = Mutex::new(None);
        let mut nonce_value = 30_000;
        let ((host, viewer), ()) = Box::pin(support::both(
            support::both(
                async {
                    let host = host
                        .open(Duration::from_millis(1), |approval, role| {
                            assert_eq!(role, Role::RequestControl);
                            *pending.lock().unwrap() = Some(approval);
                            Ok(())
                        })
                        .await
                        .unwrap();
                    assert!(local_consent.load(Ordering::Acquire));
                    host.publish_controlled_display(
                        super::launch(WorkerRole::Capture),
                        PublisherPolicy::default(),
                        super::config,
                        || {
                            nonce_value += 1;
                            Ok(nonce_value)
                        },
                    )
                    .await
                },
                viewer.observe_for_control(
                    super::launch(WorkerRole::Present),
                    ObserverPolicy::default(),
                    ClockPolicy::default(),
                    |catalog| {
                        assert!(local_consent.load(Ordering::Acquire));
                        Ok(Some(catalog.displays()[0].handle))
                    },
                    |_| {
                        notice_count.fetch_add(1, Ordering::Release);
                        Ok(())
                    },
                ),
            ),
            async {
                loop {
                    if notice_count.load(Ordering::Acquire) > 0
                        && let Some(approval) = pending.lock().unwrap().take()
                    {
                        local_consent.store(true, Ordering::Release);
                        approval.decide(true).unwrap();
                        break;
                    }
                    asupersync::time::sleep(cleanup.now(), Duration::from_millis(1)).await;
                }
            },
        ))
        .await;
        let mut host = host.unwrap();
        let mut viewer = viewer.unwrap();
        assert_eq!(notice_count.load(Ordering::Acquire), 1);
        assert_eq!(host.display(), viewer.display());
        assert_eq!(host.presentation_reports(), 0);
        assert!(!host.control().view_ready().unwrap());
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

pub(super) fn keys() -> Capabilities {
    Capabilities::default().with(Capability::Keys)
}
fn display() -> Display {
    Display {
        handle: 55,
        geometry: DisplayGeometryGeneration::INITIAL,
        x: -320,
        y: -10,
        pixel_width: 320,
        pixel_height: 240,
        logical_width: 320,
        logical_height: 240,
        scale_numerator: 1,
        scale_denominator: 1,
        rotation: 0,
    }
}
fn fixture_config(d: Display) -> Configuration {
    Configuration {
        width: d.pixel_width,
        height: d.pixel_height,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
pub(super) fn fixture_launch(role: WorkerRole, mode: &str) -> Launch {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "fr-control-bootstrap-{}-{id}.py",
        std::process::id()
    ));
    let catalog = Catalog::new(1, &[display()], &ProtocolLimits::ABSOLUTE).unwrap();
    let bytes =
        fr_media::worker::capture::monitors::encode_catalog(catalog, &ProtocolLimits::ABSOLUTE)
            .unwrap();
    let mut hex = String::new();
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut hex, "{byte:02x}").unwrap();
    }
    std::fs::write(
        &path,
        include_str!("handoff_worker_fixture.py")
            .replace("@CATALOG@", &hex)
            .replace("@MODE@", mode),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    Launch::new(&path, ":0", None, role, id as u128).unwrap()
}
async fn prepare(
    c: &Cx,
    h: &Cx,
    mode: &str,
    requested: Capabilities,
) -> (
    session_startup::NativePublisher,
    session_startup::NativeObserver,
) {
    let (mut host, viewer) = pair_initialized(c, h, capabilities(), |_| {}).await;
    let authority = host.observation().unwrap();
    assert!(!authority.view_ready().unwrap(), "manual readiness bypass");
    let chosen = Cell::new(0);
    let configured = Cell::new(0);
    let mut n = 9000;
    let (host, viewer) = Box::pin(support::both(
        host.publish_controlled_display(
            fixture_launch(WorkerRole::Capture, mode),
            PublisherPolicy::default(),
            |d| {
                configured.set(configured.get() + 1);
                Ok(fixture_config(d))
            },
            || {
                n += 1;
                Ok(n)
            },
        ),
        viewer.observe_for_control(
            fixture_launch(WorkerRole::Present, mode),
            ObserverPolicy::default(),
            ClockPolicy::default(),
            |catalog| {
                chosen.set(chosen.get() + 1);
                Ok(Some(catalog.displays()[0].handle))
            },
        ),
    ))
    .await;
    let host = host.unwrap();
    let viewer = viewer.unwrap();
    assert_eq!(configured.get(), 1);
    assert_eq!(chosen.get(), 1);
    assert_eq!(host.display(), viewer.display());
    assert_ne!(
        host.display().handle,
        display().handle,
        "native identity exposed without aliasing"
    );
    assert_eq!(
        viewer.control_request(1, requested).unwrap().target.bounds,
        InputBounds::new(DesktopPoint { x: -320, y: -10 }, 320, 240).unwrap()
    );
    assert_eq!(
        viewer.control_request(1, requested).unwrap().target.view,
        host.control_target(keys()).unwrap().view
    );
    assert_eq!(
        viewer
            .control_request(1, requested)
            .unwrap()
            .target
            .capabilities,
        requested
    );
    assert!(
        !authority.view_ready().unwrap(),
        "decode completion granted readiness"
    );
    assert_eq!(host.presentation_reports(), 0);
    (host, viewer)
}
struct Sink {
    effects: Arc<Mutex<Vec<Operation>>>,
    release_at: Arc<AtomicU64>,
    cx: Cx,
}
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        if matches!(
            op,
            Operation::Key {
                transition: KeyTransition::Release,
                ..
            }
        ) {
            self.release_at.store(
                self.cx.timer_driver().unwrap().now().as_nanos() / 1000,
                Ordering::Release,
            );
        }
        self.effects.lock().unwrap().push(op);
        Submission::Submitted
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Case {
    Idle,
    NoConsent,
    NoVisibility,
    NoMapping,
    WrongLocalTarget,
    CaptureStall,
}
pub(super) fn key(down: bool) -> Action<'static> {
    Action::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: if down {
            KeyTransition::Press
        } else {
            KeyTransition::Release
        },
    }
}
#[allow(clippy::too_many_lines)]
async fn exercise(c: Cx, h: Cx, case: Case) {
    let cleanup = Cx::current().unwrap();
    let (mut host, mut viewer) = Box::pin(prepare(
        &c,
        &h,
        if case == Case::CaptureStall {
            "stall"
        } else {
            "unchanged"
        },
        keys(),
    ))
    .await;
    let host_pid = host.worker_id();
    let viewer_pid = viewer.worker_id();
    let target = host.control_target(keys()).unwrap();
    let request = viewer.control_request(1, keys()).unwrap();
    let seat = Seat::default();
    let stop = host.control();
    let view_stop = viewer.control();
    let effects = Arc::new(Mutex::new(Vec::new()));
    let released = Arc::new(AtomicU64::new(0));
    let factories = Arc::new(AtomicUsize::new(0));
    let (send_driver, mut receive_driver) = asupersync::channel::mpsc::channel::<Driver>(1);
    let host_done = Cell::new(false);
    let viewer_done = Cell::new(false);
    let receipts = Cell::new(0);
    let ready = Cell::new(false);
    let mut activated = None;
    let mut shown = None;
    let mut actions_sent = 0;
    let mut nonce_value = 20_000;
    let mut ticket = 1000;
    let start = now(&h).unwrap();
    let ((a, b), ()) = Box::pin(support::both(
        support::both(
            async {
                let outcome = host
                    .serve_accepting_control(
                        seat.clone(),
                        |state| {
                            if let HostControlState::Pending(mut pending) = state {
                                assert!(effects.lock().unwrap().is_empty());
                                if pending.request().is_some()
                                    && pending.native_status().is_none()
                                    && pending.view_ready()?
                                {
                                    ready.set(true);
                                    if case != Case::NoConsent {
                                        let native_effects = effects.clone();
                                        let native_released = released.clone();
                                        let factory_count = factories.clone();
                                        let cx = h.clone();
                                        let driver = pending.approve(
                                            target,
                                            || {
                                                Some((
                                                    InputLeaseId::from_raw(19),
                                                    InputTicketId::from_raw(23),
                                                ))
                                            },
                                            move || {
                                                factory_count.fetch_add(1, Ordering::Relaxed);
                                                Ok(Sink {
                                                    effects: native_effects,
                                                    release_at: native_released,
                                                    cx,
                                                })
                                            },
                                            |_| true,
                                        )?;
                                        assert!(send_driver.try_send(driver).is_ok());
                                    }
                                }
                            }
                            let mut current = target;
                            if case == Case::WrongLocalTarget {
                                current.bounds =
                                    InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240)
                                        .unwrap();
                            }
                            Ok(Some(current))
                        },
                        || {
                            nonce_value += 1;
                            Ok(nonce_value)
                        },
                        || {
                            ticket += 1;
                            Some(InputTicketId::from_raw(ticket))
                        },
                    )
                    .await;
                host_done.set(true);
                view_stop.stop();
                outcome
            },
            async {
                let outcome = viewer
                    .serve_requesting_control(
                        1,
                        keys(),
                        InputPolicy::default(),
                        |state, frame| {
                            match state {
                                ViewerControlState::Requesting(pending) => {
                                    if case != Case::NoMapping {
                                        pending
                                            .confirm_mapping(request.parent, request.target.view)
                                            .unwrap();
                                    }
                                    if case != Case::NoVisibility
                                        && let Some(p) = pending.presentation()
                                        && shown != Some(p.frame)
                                    {
                                        assert_eq!(
                                            p.stage,
                                            crate::media::PresentationStage::SubmittedToCompositor
                                        );
                                        pending.visible(p.frame.as_raw()).unwrap();
                                        shown = Some(p.frame);
                                    }
                                }
                                ViewerControlState::Controlled(v) => {
                                    assert!(matches!(case, Case::Idle | Case::CaptureStall));
                                    let since = *activated.get_or_insert_with(|| now(&c).unwrap());
                                    if let Some(p) = frame {
                                        v.visible(p.frame.as_raw()).unwrap();
                                    }
                                    if actions_sent == 0 {
                                        let _ = v.action(key(true)).unwrap();
                                        actions_sent = 1;
                                    } else if actions_sent == 1
                                        && receipts.get() == 1
                                        && case == Case::Idle
                                    {
                                        let _ = v.action(key(false)).unwrap();
                                        actions_sent = 2;
                                    }
                                    if case == Case::Idle
                                        && receipts.get() == 2
                                        && now(&c).unwrap() >= since + 3_100_000
                                    {
                                        stop.revoke();
                                        view_stop.stop();
                                    }
                                }
                                ViewerControlState::Observing => panic!("lost control intent"),
                            }
                            Ok(())
                        },
                        |_| receipts.set(receipts.get() + 1),
                    )
                    .await;
                viewer_done.set(true);
                stop.revoke();
                outcome
            },
        ),
        async {
            let mut driver = None;
            let mut complete = false;
            loop {
                if driver.is_none()
                    && let Ok(d) = receive_driver.try_recv()
                {
                    driver = Some(d);
                }
                if let Some(d) = &mut driver {
                    std::future::poll_fn(|cx| {
                        if !complete && let Poll::Ready(result) = Pin::new(&mut *d).poll(cx) {
                            assert!(result.handoff_safe());
                            complete = true;
                        }
                        Poll::Ready(())
                    })
                    .await;
                }
                if host_done.get() && viewer_done.get() && (driver.is_none() || complete) {
                    break;
                }
                asupersync::time::sleep(cleanup.now(), Duration::from_millis(1)).await;
            }
        },
    ))
    .await;
    assert!(a.is_err() && b.is_err());
    if matches!(case, Case::Idle | Case::CaptureStall) {
        assert!(activated.is_some(), "host={a:?} viewer={b:?}");
        assert_eq!(factories.load(Ordering::Relaxed), 1);
        assert_eq!(
            effects.lock().unwrap().len(),
            2,
            "one native press and one release"
        );
        assert_eq!(receipts.get(), if case == Case::Idle { 2 } else { 1 });
        assert_eq!(host.statistics().encoded_updates, 0);
        assert_eq!(
            viewer.statistics().decoded,
            0,
            "initial frame was unnecessarily decoded again"
        );
        if case == Case::CaptureStall {
            let at = released.load(Ordering::Acquire);
            assert!(
                at >= start && at < start + 1_000_000,
                "held key survived until capture watchdog"
            );
        } else {
            assert!(host.presentation_reports() > 3 && viewer.presentation_reports() > 3);
            assert!(host.statistics().unchanged_observations > 3);
        }
    } else {
        assert!(activated.is_none());
        assert_eq!(receipts.get(), 0);
        assert!(effects.lock().unwrap().is_empty());
        if case == Case::NoMapping {
            assert_eq!(factories.load(Ordering::Relaxed), 1);
        } else {
            assert_eq!(factories.load(Ordering::Relaxed), 0);
        }
        if case == Case::NoConsent {
            assert!(ready.get());
        }
    }
    assert!(!seat.is_occupied());
    assert_eq!(host.worker_id(), host_pid);
    assert_eq!(viewer.worker_id(), viewer_pid);
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
}

#[test]
fn selected_display_bounds_cannot_silently_move_the_native_control_target() {
    run(|c, h| exercise(c, h, Case::WrongLocalTarget));
}
#[test]
fn bootstrap_grant_cannot_confirm_missing_local_mapping() {
    run(|c, h| exercise(c, h, Case::NoMapping));
}
#[test]
fn stalled_capture_releases_held_key_before_native_worker_deadline() {
    run(|c, h| exercise(c, h, Case::CaptureStall));
}

#[test]
fn control_bootstrap_never_replaces_an_existing_clock_owner() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair_initialized(&c, &h, capabilities(), |_| {}).await;
        host.enable_clock_sync().unwrap();
        viewer.enable_clock_sync(ClockPolicy::default()).unwrap();
        let host_attempt = host.publish_controlled_display(
            super::launch(WorkerRole::Capture),
            PublisherPolicy::default(),
            |_| panic!("native configuration after clock conflict"),
            || panic!("entropy"),
        );
        let viewer_attempt = viewer.observe_for_control(
            super::launch(WorkerRole::Present),
            ObserverPolicy::default(),
            ClockPolicy::default(),
            |_| panic!("selection after clock conflict"),
        );
        assert!(matches!(
            host_attempt.await,
            Err(session_startup::PublisherError::Clock(
                crate::media::clock::Error::Configuration
            ))
        ));
        assert!(matches!(
            viewer_attempt.await,
            Err(session_startup::ObserverError::Clock(
                crate::media::clock::Error::Configuration
            ))
        ));
    });
}

#[test]
fn abandoning_prepared_service_futures_drops_no_authority_into_a_reusable_wrapper() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (mut host, mut viewer) = Box::pin(prepare(&c, &h, "unchanged", keys())).await;
        let seat = Seat::default();
        let authority = host.control();
        drop(host.serve_accepting_control(
            seat.clone(),
            |_| panic!("local UI ran"),
            || panic!("nonce"),
            || panic!("ticket"),
        ));
        drop(viewer.serve_requesting_control(
            1,
            keys(),
            InputPolicy::default(),
            |_, _| panic!("viewer UI ran"),
            |_| panic!("receipt"),
        ));
        assert!(authority.check().is_err());
        assert!(c.checkpoint().is_err());
        assert!(!seat.is_occupied());
        assert!(
            viewer
                .serve_requesting_control(
                    1,
                    keys(),
                    InputPolicy::default(),
                    |_, _| panic!("replay"),
                    |_| {}
                )
                .await
                .is_err()
        );
        assert!(
            host.serve_accepting_control(seat, |_| panic!("replay"), || panic!("nonce"), || None)
                .await
                .is_err()
        );
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
fn prepared_viewer_request_budget_starts_at_wrapper_call_not_first_poll() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (mut host, mut viewer) = Box::pin(prepare(&c, &h, "unchanged", keys())).await;
        let request = viewer.serve_requesting_control(
            1,
            keys(),
            InputPolicy::default(),
            |_, _| panic!("expired request reached UI"),
            |_| panic!("receipt"),
        );
        asupersync::time::sleep(cleanup.now(), Duration::from_millis(2050)).await;
        assert_eq!(
            request.await,
            Err(session_startup::ObserverError::Streaming(
                crate::session_startup::StreamingViewerError::Control(
                    crate::session_startup::ControlledViewerError::Expired,
                )
            ))
        );
        assert_eq!(host.presentation_reports(), 0);
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
fn native_scope_rejects_foreign_coordinates_and_every_changed_view_generation() {
    let d = display();
    let binding = decoder::Binding {
        parent: ControlBinding {
            id: 8,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(13),
        },
        display: d.handle,
        geometry: d.geometry,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    };
    let original = target(d, binding, keys()).unwrap();
    check_target(d, binding, original).unwrap();
    for index in 0..7 {
        let mut changed = original;
        match index {
            0 => changed.display_binding += 1,
            1 => changed.bounds = InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            2 => {
                changed.bounds =
                    InputBounds::new(DesktopPoint { x: -320, y: -10 }, 321, 240).unwrap();
            }
            3 => changed.view.geometry = changed.view.geometry.next().unwrap(),
            4 => changed.view.viewport = changed.view.viewport.next().unwrap(),
            5 => changed.view.configuration = changed.view.configuration.next().unwrap(),
            6 => changed.view.recovery = changed.view.recovery.next().unwrap(),
            _ => unreachable!(),
        }
        assert_eq!(
            check_target(d, binding, changed),
            Err(WireError::InvalidBinding)
        );
    }
    // This helper does not pretend to probe platform capabilities or grant them.
    check_target(
        d,
        binding,
        target(d, binding, keys().with(Capability::Absolute)).unwrap(),
    )
    .unwrap();
}

mod managed;
