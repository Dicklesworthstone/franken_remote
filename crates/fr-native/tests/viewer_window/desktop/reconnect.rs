//! Actual TLS/UDP, XCB and supervised child lifetimes. The child replays the
//! existing recorded HEVC corpus; capture/decoder replies are explicit fixtures,
//! not hardware, optical visibility, admission or automatic-input evidence.
use super::*;
use fr_native::desktop::reconnect::{CleanupFailure, Mode, Session, Ui};
use frd::native_connection::reconnect::{Application, CallbackError, Failure, Status, retryable};
use std::{
    cell::RefCell,
    future::Future,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    rc::Rc,
    task::{Context, Poll, Waker},
};

#[derive(Default)]
struct StateLog {
    choices: Vec<u8>,
    ready: Vec<u8>,
    windows: Vec<fr_native::viewer_window::WindowControl>,
    workers: Vec<u32>,
    frames: usize,
}
struct Interface {
    log: Rc<RefCell<StateLog>>,
    panic_on_choice: bool,
    fail_status: bool,
}
impl Ui for Interface {
    fn choose(
        &mut self,
        attempt: u8,
        catalog: &display::Catalog,
    ) -> Result<Option<u128>, CallbackError> {
        assert!(!self.panic_on_choice, "explicit reconnect UI panic");
        assert_eq!(catalog.displays().len(), 1);
        self.log.borrow_mut().choices.push(attempt);
        Ok(Some(9))
    }
    fn approval(
        &mut self,
        _: u8,
        _: fr_client::startup::ApprovalNotice,
    ) -> Result<(), CallbackError> {
        panic!("this scripted peer has no approval notification")
    }
    fn ready(&mut self, attempt: u8, desktop: &mut ClientDesktop) -> Result<(), CallbackError> {
        let mut log = self.log.borrow_mut();
        // Old UI handles must not fence the new original session, even when
        // the X server reuses a numeric drawable ID on a new connection.
        for old in &log.windows {
            old.stop();
            assert!(matches!(
                old.status(),
                fr_native::viewer_window::Status::Stopped(_)
            ));
        }
        assert_eq!(desktop.state(), State::Viewing);
        log.ready.push(attempt);
        log.windows.push(desktop.window().unwrap());
        log.workers.push(desktop.worker_id().unwrap());
        assert!(desktop.input().is_none());
        Ok(())
    }
    fn frame(
        &mut self,
        _: u8,
        _: Option<frd::session_startup::Presentation>,
    ) -> Result<(), CallbackError> {
        self.log.borrow_mut().frames += 1;
        Ok(())
    }
    fn status(&mut self, _: Status) -> Result<(), CallbackError> {
        if self.fail_status {
            Err(CallbackError)
        } else {
            Ok(())
        }
    }
}
fn application(
    display: &str,
    image: &Path,
    epoch: u128,
) -> (Session<Interface>, Rc<RefCell<StateLog>>) {
    let log = Rc::new(RefCell::new(StateLog::default()));
    let session = Session::new(
        Configuration::new(image, display, None, epoch).unwrap(),
        ObserverPolicy {
            timeout: Duration::from_millis(900),
            ..ObserverPolicy::default()
        },
        Mode::Observe,
        Interface {
            log: log.clone(),
            panic_on_choice: false,
            fail_status: false,
        },
    );
    (session, log)
}
#[test]
fn unpolled_reconnect_attempt_has_no_native_owner_and_cancels_only_its_viewer() {
    let runtime = support::runtime();
    let original = viewer(&runtime);
    let stop = original.control();
    let foreign = viewer(&runtime);
    let (mut session, log) = application(":0", Path::new("/usr/bin/false"), 100);
    drop(session.run(1, original));
    assert!(stop.is_stopped());
    assert!(!foreign.control().is_stopped());
    assert!(session.desktop().is_none());
    assert_eq!(log.borrow().choices, [] as [u8; 0]);
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
    assert_eq!(runtime.block_on(session.cleanup(&cx, deadline)), Ok(()));
    assert!(session.last_cleanup().is_none());
}
#[test]
fn pending_original_attempt_is_fenced_and_no_renderer_entry_proves_native_absence() {
    let runtime = support::runtime();
    let original = viewer(&runtime);
    let stop = original.control();
    let (mut session, log) = application(":0", Path::new("/usr/bin/false"), 100);
    let mut running = Box::pin(session.run(1, original));
    assert!(matches!(
        running
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    drop(running);
    assert!(stop.is_stopped());
    assert_eq!(session.desktop().unwrap().state(), State::Stopped);
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
    drop(session.cleanup(&cx, deadline));
    assert!(session.desktop().is_some());
    assert!(cx.checkpoint().is_ok());
    assert_eq!(runtime.block_on(session.cleanup(&cx, deadline)), Ok(()));
    assert_eq!(session.cleanup_failure(), None);
    // The one-use renderer factory was never entered, so None is backed by
    // positive no-launch evidence rather than dropping an unknown child.
    assert!(session.desktop().is_none());
    assert!(matches!(session.last_cleanup().unwrap().media, Ok(None)));
    let replacement = viewer(&runtime);
    let replacement_stop = replacement.control();
    assert_eq!(
        runtime.block_on(session.run(1, replacement)),
        Err(ObserverError::Order)
    );
    assert!(replacement_stop.is_stopped());
    assert_eq!(log.borrow().choices, [] as [u8; 0]);
}
#[test]
fn interrupted_bootstrap_collects_its_window_without_inventing_a_decoder() {
    let native = Desktop::start();
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    let (mut session, log) = application(&native.display, Path::new("/usr/bin/false"), 100);
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
            select_peer(q, &h, routes, stop.clone(), true),
        ))
        .await;
        assert!(result.is_err());
        assert!(stop.is_stopped());
        assert_eq!(log.borrow().choices, [1]);
        assert_eq!(log.borrow().ready, [] as [u8; 0]);
        assert!(session.desktop().unwrap().window().is_some());
        let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
        let deadline = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
        assert_eq!(session.cleanup(&cleanup, deadline).await, Ok(()));
        assert_eq!(session.cleanup_failure(), None);
        // This peer stopped after display selection, before worker spawn. The
        // registered Launch was dropped, a positive NotStarted receipt.
        assert!(matches!(session.last_cleanup().unwrap().media, Ok(None)));
        assert!(matches!(
            session.last_cleanup().unwrap().window,
            WindowCleanup::Complete(_)
        ));
        assert!(session.desktop().is_none());
        assert!(h.checkpoint().is_ok());
    });
}
#[test]
fn cleanup_keeps_its_original_deadline_and_cancellation_context() {
    for cancel in [false, true] {
        let runtime = support::runtime();
        let (mut session, _) = application(":0", Path::new("/usr/bin/false"), 100);
        let original = viewer(&runtime);
        let mut running = Box::pin(session.run(1, original));
        let _ = running
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
        drop(running);
        let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
        let deadline = Deadline::after(&cleanup, Duration::from_millis(5)).unwrap();
        let future = session.cleanup(&cleanup, deadline);
        if cancel {
            cleanup.cancel_fast(asupersync::types::CancelKind::User);
        } else {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(runtime.block_on(future), Err(CallbackError));
        assert_eq!(
            session.cleanup_failure(),
            Some(if cancel {
                CleanupFailure::Cancelled
            } else {
                CleanupFailure::Expired
            })
        );
        assert!(session.desktop().is_some());
        assert_eq!(cleanup.checkpoint().is_err(), cancel);
    }
}
#[test]
fn caught_callback_panic_fences_even_while_the_failed_run_future_remains_retained() {
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    let (client, host) = runtime.block_on(support::native_pair(&c, "localhost", ALPN));
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
    let log = Rc::new(RefCell::new(StateLog::default()));
    let mut session = Session::new(
        Configuration::new(Path::new("/usr/bin/false"), ":0", None, 100).unwrap(),
        ObserverPolicy::default(),
        Mode::Observe,
        Interface {
            log,
            panic_on_choice: true,
            fail_status: false,
        },
    );
    let mut running = Box::pin(session.run(1, viewer));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(Box::pin(support::both(
            running.as_mut(),
            select_peer(q, &h, routes, stop.clone(), false),
        )))
    }));
    assert!(result.is_err());
    assert!(stop.is_stopped());
    drop(running);
    assert_eq!(session.desktop().unwrap().state(), State::Stopped);
    assert!(h.checkpoint().is_ok());
}
fn image() -> PathBuf {
    decoder_image("healthy")
}
fn decoder_image(mode: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-desktop-reconnect-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    // Extend only this test's copy for the public plain-capture and selected-
    // presentation requests. The production binary contains no fixture path.
    let source = include_str!(
        "../../../../frd/src/session_startup/native_control/worker_fixture.py"
    )
    .replace(
        "elif k == 8:",
        "elif k == 1: reply(h,257,b)\n        elif k == 14: reply(h,272,b)\n        elif k == 8:",
    );
    let source = match mode {
        "exit-configure" => source.replace(
            "elif k == 14: reply(h,272,b)",
            "elif k == 14: raise SystemExit(7)",
        ),
        "stall-configure" => source.replace(
            "elif k == 14: reply(h,272,b)",
            "elif k == 14: __import__('time').sleep(60)",
        ),
        "exit-decode" => source.replace(
            "elif k in (4,6): reply(h,261 if k==4 else 264,b[:8])",
            "elif k in (4,6): raise SystemExit(7)",
        ),
        "healthy" => source,
        _ => panic!("unknown explicit decoder fixture"),
    };
    std::fs::write(&path, source).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

async fn attach_media(
    q: &mut QuicRecords,
    h: &Cx,
    routes: ControlRoutes,
    parent: ControlBinding,
    selection: &negotiation::Selection,
) -> frd::media_quic::NegotiatedMedia {
    use fr_core::ids::*;
    use fr_transport::quic::{ChannelRequest, ChannelScope};
    use fr_wire::{
        attachment::{MediaRole, Ticket},
        decoder::Binding,
    };
    let binding = Binding {
        parent: ControlBinding { id: 8, ..parent },
        display: 9,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    };
    let mut attached = Vec::new();
    for (index, role) in [
        (8, MediaRole::Configuration),
        (9, MediaRole::Recovery),
        (10, MediaRole::Video),
    ] {
        let mut channel = q
            .offer_media_role(
                h,
                ChannelScope {
                    control: routes,
                    parent,
                    selection,
                },
                ChannelRequest {
                    binding: Binding {
                        parent: ControlBinding {
                            id: index,
                            ..parent
                        },
                        ..binding
                    },
                    ticket: Ticket(u128::from(index) + 100),
                    timeout: Duration::from_secs(2),
                },
                role,
                || true,
            )
            .unwrap();
        while !channel.is_complete() {
            channel.transmit(q, h, || true).unwrap();
            channel.dispatch(q, h, || true).unwrap();
            if channel.finish(q, h, || true).unwrap().is_some() {
                break;
            }
            q.drive(h, Duration::from_millis(1), || true).await.unwrap();
        }
        attached.push(channel);
    }
    frd::media_quic::NegotiatedMedia::new(q, selection, &attached[0], &attached[1], &attached[2])
        .unwrap()
}
async fn media_peer(q: QuicRecords, h: &Cx, cleanup: &Cx, routes: ControlRoutes, image: &Path) {
    Box::pin(media_peer_until(q, h, cleanup, routes, image, None)).await;
}
async fn media_peer_until(
    q: QuicRecords,
    h: &Cx,
    cleanup: &Cx,
    routes: ControlRoutes,
    image: &Path,
    stopped: Option<frd::session_startup::StreamingViewerControl>,
) {
    use fr_core::{
        authority::{AuthorityPolicy, SessionAuthority},
        ids::*,
    };
    use fr_media::{
        delivery::SendPolicy,
        worker::{Backend, Configuration, Role},
    };
    use frd::{
        media::{CaptureSource, ObservationControl, decoder_startup},
        media_egress::Lane,
        worker::Launch,
    };
    let (mut q, routes, parent) = Box::pin(startup_peer(q, h, routes, true)).await;
    let selection = offered().select().unwrap();
    let channels = attach_media(&mut q, h, routes, parent, &selection).await;
    let mut authority =
        SessionAuthority::new(parent.remote_session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority
        .authorize_observation(frd::media::host_now(h).unwrap())
        .unwrap();
    let control = ObservationControl::new(h.clone(), authority).unwrap();
    let configuration = Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    };
    let mut source = CaptureSource::start(
        &control,
        Launch::new(image, ":0", None, Role::Capture, 501).unwrap(),
        configuration,
    )
    .await
    .unwrap();
    let update = source.capture_if_changed(&control, true).await.unwrap();
    let setup = channels.decoder_setup(&q, Duration::from_secs(2)).unwrap();
    let mut startup =
        decoder_startup::Host::new(control.clone(), &q, setup, configuration, update).unwrap();
    let mut sender = channels
        .sender(&q, control.clone(), SendPolicy::default())
        .unwrap();
    let mut configured = false;
    while !startup.is_complete() {
        if stopped
            .as_ref()
            .is_some_and(frd::session_startup::StreamingViewerControl::is_stopped)
        {
            break;
        }
        if !configured {
            configured = startup.transmit(&mut q).unwrap();
        }
        startup.dispatch(&mut q).unwrap();
        if let Some(recovery) = startup.take_recovery().unwrap() {
            sender.enqueue_capture(recovery).unwrap();
        }
        sender.transmit(h, &mut q, Lane::Original).unwrap();
        q.drive(h, Duration::from_millis(1), || true).await.unwrap();
    }
    // Deliberate link loss: no observation renewal reply follows. The viewer
    // must expire its original authority, return a retryable reason and reap.
    q.close();
    control.revoke();
    source.worker_mut().abort();
    source
        .worker_mut()
        .reap(
            cleanup,
            Deadline::after(cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
}
#[test]
fn completed_desktop_reconnects_only_after_reap_with_fresh_workers_and_terminal_old_handles() {
    let native = Desktop::start();
    let runtime = support::runtime();
    let path = image();
    let (mut session, log) = application(&native.display, &path, 300);
    for attempt in [1, 2] {
        let c = runtime.request_cx_with_budget(Budget::INFINITE);
        let h = runtime.request_cx_with_budget(Budget::INFINITE);
        let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
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
                    media_peer(q, &h, &cleanup, routes, &path),
                )),
            )
            .await
            .expect("bounded desktop reconnect integration");
            assert!(retryable(Failure::Observation(result.unwrap_err())));
            assert!(stop.is_stopped());
            assert_eq!(log.borrow().ready.len(), usize::from(attempt));
            assert!(log.borrow().frames > 0);
            assert!(session.desktop().is_some());
            let deadline = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
            assert_eq!(session.cleanup(&cleanup, deadline).await, Ok(()));
            assert!(session.desktop().is_none());
            assert_eq!(session.cleanup_failure(), None);
            let report = session.last_cleanup().unwrap();
            assert!(matches!(report.media, Ok(Some(_))));
            assert!(matches!(report.window, WindowCleanup::Complete(_)));
            assert_eq!(
                report.input,
                frd::session_startup::viewer_events::CaptureCleanup::NotStarted
            );
            assert_eq!(
                report.clipboard,
                Ok(frd::native_clipboard::Cleanup::NotStarted)
            );
            assert!(cleanup.checkpoint().is_ok());
        });
    }
    let log = log.borrow();
    assert_eq!(log.choices, [1, 2]);
    assert_eq!(log.ready, [1, 2]);
    assert_ne!(log.workers[0], log.workers[1]);
    for window in &log.windows {
        assert!(matches!(
            window.status(),
            fr_native::viewer_window::Status::Stopped(_)
        ));
    }
}

#[path = "reconnect/picker.rs"]
mod picker;

#[test]
fn failed_decoder_configuration_remains_collectable_from_the_public_desktop() {
    incomplete_decoder("exit-configure");
}
#[test]
fn timed_out_decoder_configuration_remains_collectable_from_the_public_desktop() {
    incomplete_decoder("stall-configure");
}
#[test]
fn failed_first_decode_remains_collectable_from_the_public_desktop() {
    incomplete_decoder("exit-decode");
}
fn incomplete_decoder(mode: &str) {
    let native = Desktop::start();
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
    let capture = image();
    let decoder = decoder_image(mode);
    let (mut session, log) = application(&native.display, &decoder, 901);
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
            Duration::from_secs(5),
            Box::pin(support::both(
                session.run(1, viewer),
                media_peer_until(q, &h, &cleanup, routes, &capture, Some(stop.clone())),
            )),
        )
        .await
        .expect("bounded failed bootstrap");
        assert!(result.is_err());
        assert!(stop.is_stopped());
        assert_eq!(log.borrow().ready.len(), 0);
        assert_eq!(log.borrow().frames, 0);
        let window = session.desktop().unwrap().window().unwrap();
        // No NativeObserver was returned, yet its nested decoder child must be
        // explicitly reaped. Never replace this assertion with absent == safe.
        assert_eq!(
            session
                .cleanup(
                    &cleanup,
                    Deadline::after(&cleanup, Duration::from_secs(1)).unwrap()
                )
                .await,
            Ok(())
        );
        let report = session.last_cleanup().unwrap();
        assert!(matches!(report.media, Ok(Some(status)) if !status.success()));
        assert!(matches!(report.window, WindowCleanup::Complete(_)));
        assert_eq!(
            report.input,
            frd::session_startup::viewer_events::CaptureCleanup::NotStarted
        );
        assert_eq!(
            report.clipboard,
            Ok(frd::native_clipboard::Cleanup::NotStarted)
        );
        assert!(session.desktop().is_none());
        assert!(matches!(
            window.status(),
            fr_native::viewer_window::Status::Stopped(_)
        ));
        assert!(cleanup.checkpoint().is_ok());
        // The scripted host deliberately revoked its own observation after
        // the viewer stopped; only the independent cleanup context stays live.
        assert!(h.checkpoint().is_err());
    });
}
