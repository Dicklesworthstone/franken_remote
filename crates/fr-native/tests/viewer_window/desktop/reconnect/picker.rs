//! Original Desktop/Session, real native chooser and localhost TLS/UDP.
//! Media in the positive reconnect case remains the explicit existing fixture.
use super::*;
use fr_native::{
    desktop::PickerCleanup,
    display_picker::{Control as PickerControl, Error as PickerError, Status as PickerStatus},
};

struct Driver(Child);
impl Driver {
    fn start(display: &str, gesture: &str) -> Self {
        Self(
            Command::new("python3")
                .arg(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/viewer_window/picker_driver.py"),
                )
                .args([display, "0", gesture])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        )
    }
    fn finish(&mut self) -> u32 {
        assert!(self.0.wait().unwrap().success());
        let mut id = String::new();
        self.0
            .stdout
            .take()
            .unwrap()
            .take(32)
            .read_to_string(&mut id)
            .unwrap();
        id.trim().parse().unwrap()
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct PickingUi {
    old: Rc<RefCell<Vec<PickerControl>>>,
    inner: Interface,
}
impl Ui for PickingUi {
    fn choose(&mut self, _: u8, _: &display::Catalog) -> Result<Option<u128>, CallbackError> {
        panic!("the native picker must replace, not supplement, a guessed alias")
    }
    fn approval(
        &mut self,
        _: u8,
        _: fr_client::startup::ApprovalNotice,
    ) -> Result<(), CallbackError> {
        Ok(())
    }
    fn ready(&mut self, attempt: u8, desktop: &mut ClientDesktop) -> Result<(), CallbackError> {
        let picker = desktop.picker().unwrap();
        assert_eq!(picker.status(), PickerStatus::Consumed);
        assert_eq!(desktop.picker_cleanup(), PickerCleanup::Complete);
        for old in self.old.borrow().iter() {
            old.cancel();
        }
        picker.cancel();
        assert_eq!(desktop.state(), State::Viewing);
        self.old.borrow_mut().push(picker);
        self.inner.ready(attempt, desktop)
    }
    fn frame(
        &mut self,
        attempt: u8,
        frame: Option<frd::session_startup::Presentation>,
    ) -> Result<(), CallbackError> {
        self.inner.frame(attempt, frame)
    }
}
fn picking(display: &str, image: &Path) -> Session<PickingUi> {
    Session::new(
        Configuration::new(image, display, None, 900)
            .unwrap()
            .with_display_picker(),
        ObserverPolicy {
            timeout: Duration::from_millis(900),
            ..ObserverPolicy::default()
        },
        Mode::Observe,
        PickingUi {
            old: Rc::new(RefCell::new(Vec::new())),
            inner: Interface {
                log: Rc::new(RefCell::new(StateLog::default())),
                panic_on_choice: false,
                fail_status: false,
            },
        },
    )
}
#[test]
fn original_native_picker_is_recreated_and_joined_before_each_real_viewing_attempt() {
    let native = Desktop::start();
    let runtime = support::runtime();
    let image = image();
    let mut session = picking(&native.display, &image);
    for attempt in [1, 2] {
        let c = runtime.request_cx_with_budget(Budget::INFINITE);
        let h = runtime.request_cx_with_budget(Budget::INFINITE);
        let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
        let mut driver = Driver::start(&native.display, "click-0");
        runtime.block_on(async {
            let (client, host) = support::native_pair(&c, "localhost", ALPN).await;
            let viewer = Viewer::new(
                c.clone(),
                client.unwrap(),
                offered(),
                Policy::default(),
                Duration::from_secs(2),
            )
            .unwrap();
            let stop = viewer.control();
            let (q, routes) = QuicRecords::bootstrap(host.unwrap(), &h, Policy::default()).unwrap();
            let (result, ()) = asupersync::time::timeout(
                c.timer_driver().unwrap().now(),
                Duration::from_secs(8),
                Box::pin(support::both(
                    session.run(attempt, viewer),
                    media_peer(q, &h, &cleanup, routes, &image),
                )),
            )
            .await
            .unwrap();
            assert!(retryable(Failure::Observation(result.unwrap_err())));
            assert!(stop.is_stopped());
            let desktop = session.desktop().unwrap();
            assert!(desktop.window().is_some());
            assert_eq!(desktop.display().unwrap().handle, 9);
            assert_eq!(desktop.picker().unwrap().status(), PickerStatus::Consumed);
            assert_eq!(
                session
                    .cleanup(
                        &cleanup,
                        Deadline::after(&cleanup, Duration::from_secs(1)).unwrap()
                    )
                    .await,
                Ok(())
            );
            assert!(session.desktop().is_none());
            assert_eq!(
                session.last_cleanup().unwrap().picker,
                PickerCleanup::Complete
            );
            assert!(matches!(session.last_cleanup().unwrap().media, Ok(Some(_))));
        });
        let id = driver.finish();
        assert_eq!(native.peer(id, "exists"), "0");
    }
}
#[test]
fn genuine_picker_cancel_before_renderer_has_positive_cleanup_without_inventing_decoder_reap() {
    let native = Desktop::start();
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
    let mut session = picking(&native.display, Path::new("/usr/bin/false"));
    let mut driver = Driver::start(&native.display, "escape");
    runtime.block_on(async {
        let (client, host) = support::native_pair(&c, "localhost", ALPN).await;
        let viewer = Viewer::new(
            c.clone(),
            client.unwrap(),
            offered(),
            Policy::default(),
            Duration::from_secs(2),
        )
        .unwrap();
        let stop = viewer.control();
        let (q, routes) = QuicRecords::bootstrap(host.unwrap(), &h, Policy::default()).unwrap();
        let (result, ()) = Box::pin(support::both(
            session.run(1, viewer),
            select_peer(q, &h, routes, stop.clone(), false),
        ))
        .await;
        assert!(!retryable(Failure::Observation(result.unwrap_err())));
        assert!(stop.is_stopped());
        assert_eq!(
            session.last_error(),
            Some(DesktopError::Picker(PickerError::Cancelled))
        );
        assert!(session.desktop().unwrap().window().is_none());
        assert!(session.desktop().unwrap().worker_id().is_none());
        let expired = Deadline::after(&cleanup, Duration::from_millis(1)).unwrap();
        asupersync::time::sleep(cleanup.now(), Duration::from_millis(3)).await;
        assert_eq!(session.cleanup(&cleanup, expired).await, Err(CallbackError));
        assert_eq!(session.cleanup_failure(), Some(CleanupFailure::Expired));
        assert!(session.desktop().is_some());
        let deadline = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
        assert_eq!(session.cleanup(&cleanup, deadline).await, Ok(()));
        assert_eq!(session.cleanup_failure(), None);
        assert!(session.desktop().is_none());
        let report = session.last_cleanup().unwrap();
        assert_eq!(report.picker, PickerCleanup::Complete);
        assert!(matches!(report.media, Ok(None)));
        assert_eq!(report.window, WindowCleanup::NotStarted);
        assert_eq!(
            report.input,
            frd::session_startup::viewer_events::CaptureCleanup::NotStarted
        );
        assert_eq!(
            report.clipboard,
            Ok(frd::native_clipboard::Cleanup::NotStarted)
        );
    });
    assert_eq!(native.peer(driver.finish(), "exists"), "0");
}
#[test]
fn picker_timeout_retains_original_deadline_and_is_not_relabelled_user_cancellation() {
    let native = Desktop::start();
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
    let mut session = picking(&native.display, Path::new("/usr/bin/false"));
    runtime.block_on(async {
        let (client, host) = support::native_pair(&c, "localhost", ALPN).await;
        let viewer = Viewer::new(
            c.clone(),
            client.unwrap(),
            offered(),
            Policy::default(),
            Duration::from_secs(2),
        )
        .unwrap();
        let stop = viewer.control();
        let (q, routes) = QuicRecords::bootstrap(host.unwrap(), &h, Policy::default()).unwrap();
        let (result, ()) = Box::pin(support::both(
            session.run(1, viewer),
            select_peer(q, &h, routes, stop.clone(), false),
        ))
        .await;
        assert!(result.is_err());
        assert!(stop.is_stopped());
        assert!(session.desktop().unwrap().window().is_none());
        assert!(session.desktop().unwrap().picker().is_some());
        assert_ne!(
            session.last_error(),
            Some(DesktopError::Picker(PickerError::Cancelled))
        );
        assert_eq!(
            session
                .cleanup(
                    &cleanup,
                    Deadline::after(&cleanup, Duration::from_secs(1)).unwrap()
                )
                .await,
            Ok(())
        );
        assert_eq!(session.cleanup_failure(), None);
        assert_eq!(
            session.last_cleanup().unwrap().picker,
            PickerCleanup::Complete
        );
        assert_ne!(
            session.last_error(),
            Some(DesktopError::Picker(PickerError::Cancelled))
        );
        assert!(session.desktop().is_none());
    });
}

// Explicit peer grammar for a multi-monitor catalog and a still-pending
// approval. The application's production startup consumes the actual records.
fn multiple_catalog() -> display::Catalog {
    let first = display::Display {
        handle: u128::MAX,
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
    };
    let second = display::Display {
        handle: u128::MAX - 1,
        x: -400,
        pixel_width: 400,
        logical_width: 400,
        ..first
    };
    display::Catalog::new(7, &[first, second], &ProtocolLimits::ABSOLUTE).unwrap()
}

async fn negotiate_offer(q: &mut QuicRecords, cx: &Cx, routes: ControlRoutes) {
    assert!(matches!(
        negotiation::decode(&receive(q, cx, routes).await, negotiation::MAX_RECORD, 0).unwrap(),
        Message::ClientHello(_)
    ));
    send_message(q, cx, routes, Message::HostCapabilities(offered())).await;
    assert!(matches!(
        negotiation::decode(&receive(q, cx, routes).await, negotiation::MAX_RECORD, 0).unwrap(),
        Message::SelectedConfiguration(_)
    ));
}

async fn catalog_peer(
    mut q: QuicRecords,
    cx: &Cx,
    mut routes: ControlRoutes,
    stop: frd::session_startup::StreamingViewerControl,
    pending_approval: bool,
) {
    negotiate_offer(&mut q, cx, routes).await;
    if pending_approval {
        send_message(
            &mut q,
            cx,
            routes,
            Message::ApprovalRequired {
                request: RemoteSessionId::from_raw(13),
                deadline_us: support::clock(cx) + 3_000_000,
                role: Role::Observe,
            },
        )
        .await;
    } else {
        let binding = ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(13),
        };
        send_message(
            &mut q,
            cx,
            routes,
            Message::SessionOpened {
                binding,
                selection: offered().select().unwrap(),
                observation_until_us: support::clock(cx) + 3_000_000,
            },
        )
        .await;
        routes = q
            .bind_control(cx, routes, binding.id, negotiation::MAX_RECORD, || true)
            .unwrap();
        assert_eq!(
            negotiation::decode(
                &receive(&mut q, cx, routes).await,
                negotiation::MAX_RECORD,
                binding.id
            )
            .unwrap(),
            Message::BindingAccepted {
                binding: binding.id
            }
        );
        let catalog = multiple_catalog();
        let mut bytes = [0; display::MAX_CATALOG_BYTES];
        let n = display::encode(
            &display::Message::Catalog(catalog),
            binding,
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        send(&mut q, cx, routes, &bytes[..n]).await;
        let choice = display::decode(
            &receive(&mut q, cx, routes).await,
            binding,
            &ProtocolLimits::ABSOLUTE,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        assert_eq!(
            choice,
            display::Message::Select(catalog.selection(u128::MAX - 1).unwrap())
        );
    }
    while !stop.is_stopped() {
        // Approval has not been granted (or media is not offered). No additional
        // client application record may pretend to advance those stages.
        q.receive_ready(
            cx,
            || true,
            |_| true,
            |_, _| panic!("unexpected record before approval/media"),
        )
        .unwrap();
        if q.drive(cx, Duration::from_millis(2), || true)
            .await
            .is_err()
        {
            break;
        }
    }
}
#[test]
fn multi_display_native_choice_reaches_the_same_wire_selection_and_renderer_start() {
    let native = Desktop::start();
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    let mut desktop = ClientDesktop::new(
        Configuration::new(Path::new("/usr/bin/false"), &native.display, None, 900)
            .unwrap()
            .with_display_picker(),
    );
    let mut driver = Driver::start(&native.display, "click-1");
    runtime.block_on(async {
        let (client, host) = support::native_pair(&c, "localhost", ALPN).await;
        let viewer = Viewer::new(
            c.clone(),
            client.unwrap(),
            offered(),
            Policy::default(),
            Duration::from_secs(2),
        )
        .unwrap();
        let stop = viewer.control();
        let (q, routes) = QuicRecords::bootstrap(host.unwrap(), &h, Policy::default()).unwrap();
        let (result, ()) = Box::pin(support::both(
            desktop
                .open(
                    viewer,
                    ObserverPolicy {
                        timeout: Duration::from_millis(900),
                        ..ObserverPolicy::default()
                    },
                    None,
                    |_| panic!("not a guessed handle"),
                    |_| Ok(()),
                )
                .unwrap(),
            catalog_peer(q, &h, routes, stop.clone(), false),
        ))
        .await;
        assert!(result.is_err());
        assert!(stop.is_stopped());
        assert!(
            desktop.window().is_some(),
            "same selected display reached the renderer factory"
        );
        assert_eq!(desktop.picker().unwrap().status(), PickerStatus::Consumed);
        assert_eq!(desktop.picker_cleanup(), PickerCleanup::Complete);
        assert!(
            desktop.worker_id().is_none(),
            "no media channels were offered"
        );
    });
    assert_eq!(native.peer(driver.finish(), "exists"), "0");
    wait(|| matches!(desktop.window_cleanup(), WindowCleanup::Complete(_)));
}
#[test]
fn no_native_picker_or_renderer_exists_while_host_approval_remains_pending() {
    let native = Desktop::start();
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    let mut desktop = ClientDesktop::new(
        Configuration::new(Path::new("/usr/bin/false"), &native.display, None, 900)
            .unwrap()
            .with_display_picker(),
    );
    let notices = AtomicUsize::new(0);
    runtime.block_on(async {
        let (client, host) = support::native_pair(&c, "localhost", ALPN).await;
        let viewer = Viewer::new(
            c.clone(),
            client.unwrap(),
            offered(),
            Policy::default(),
            Duration::from_secs(2),
        )
        .unwrap();
        let stop = viewer.control();
        let (q, routes) = QuicRecords::bootstrap(host.unwrap(), &h, Policy::default()).unwrap();
        let (result, ()) = Box::pin(support::both(
            desktop
                .open(
                    viewer,
                    ObserverPolicy {
                        timeout: Duration::from_millis(300),
                        ..ObserverPolicy::default()
                    },
                    None,
                    |_| panic!("not approved"),
                    |_| {
                        notices.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    },
                )
                .unwrap(),
            catalog_peer(q, &h, routes, stop.clone(), true),
        ))
        .await;
        assert!(result.is_err());
        assert!(stop.is_stopped());
    });
    assert_eq!(notices.load(Ordering::Relaxed), 1);
    assert!(desktop.picker().is_none());
    assert!(desktop.window().is_none());
    assert!(desktop.worker_id().is_none());
    assert_eq!(desktop.picker_cleanup(), PickerCleanup::NotStarted);
}
