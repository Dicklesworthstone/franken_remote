//! Actual `WM_DELETE_WINDOW` -> running Desktop -> original TLS/UDP closing exchange.
//! The source/decoder use the explicit IPC fixture from this target, not hardware.
use super::*;
use fr_wire::{
    authority,
    closure::{self, Cleanup, Closed, ClosedReason, OutstandingEffects},
};

#[derive(Clone, Copy)]
enum End {
    Report(Closed),
    Silent,
    Emergency,
    Hidden,
}
fn report() -> Closed {
    Closed {
        reason: ClosedReason::ClientRequested,
        cleanup: Cleanup::Unconfirmed,
        effects: OutstandingEffects::Unknown,
    }
}
struct ClosingPeer {
    h: Cx,
    cleanup: Cx,
    image: PathBuf,
    log: Rc<RefCell<StateLog>>,
    frames_at_close: std::cell::Cell<usize>,
    stop: frd::session_startup::StreamingViewerControl,
}
impl ClosingPeer {
    async fn run(&self, q: QuicRecords, routes: ControlRoutes, native: &Desktop, end: End) {
        let Self {
            h,
            cleanup,
            image,
            log,
            frames_at_close,
            stop,
        } = self;
        let requested = std::cell::Cell::new(false);
        let (mut q, mut source, control, parent, routes) =
            Box::pin(start_media(q, h, routes, image, None)).await;
        // Drain startup acknowledgements, preserving actual native empty witnesses.
        for _ in 0..3 {
            q.drive(h, Duration::from_millis(1), || true).await.unwrap();
        }
        let window = log.borrow().windows[0].clone();
        let id = window.target().unwrap().window();
        match end {
            End::Emergency => window.stop(),
            End::Hidden => {
                native.peer(id, "unmap");
            }
            End::Report(_) | End::Silent => {
                native.peer(id, "close");
            }
        }
        let until = support::clock(h) + 800_000;
        // The independent X event must first be observed by its native owner.
        // Record the callback boundary only after that actual state transition,
        // not before scheduling a window-manager message on another thread.
        while window.status() == fr_native::viewer_window::Status::Mapped {
            assert!(support::clock(h) < until);
            asupersync::time::sleep(h.now(), Duration::from_millis(1)).await;
        }
        frames_at_close.set(log.borrow().frames);
        while !stop.is_stopped() && !requested.get() {
            assert!(support::clock(h) < until, "window closure hung");
            q.drive(h, Duration::from_millis(1), || true).await.unwrap();
            q.receive_ready(
                h,
                || true,
                |route| route == Route::Stream(routes.inbound),
                |_, bytes| {
                    let value = closure::decode_request(
                        bytes,
                        authority::Binding {
                            channel: parent.id,
                            session: parent.remote_session,
                        },
                        &ProtocolLimits::ABSOLUTE,
                        InputDirection::ViewerToHost,
                        InputDelivery::Reliable,
                    )
                    .unwrap();
                    assert_eq!(value.reason, closure::Reason::Requested);
                    assert!(!requested.replace(true), "close request repeated");
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        }
        match end {
            End::Report(report) => {
                assert!(
                    requested.get(),
                    "no terminal request crossed original connection; window={:?}",
                    window.status()
                );
                control.revoke();
                source.worker_mut().abort();
                let original = q.binding();
                let result = q
                    .close_with_closed(
                        cleanup,
                        &original,
                        routes.outbound,
                        authority::Binding {
                            channel: parent.id,
                            session: parent.remote_session,
                        },
                        report,
                    )
                    .await;
                assert_eq!(result, Ok(()));
            }
            End::Silent => {
                assert!(requested.get());
                while !stop.is_stopped() {
                    assert!(support::clock(h) < until);
                    q.drive(h, Duration::from_millis(1), || true).await.unwrap();
                }
            }
            End::Emergency | End::Hidden => assert!(!requested.get()),
        }
        self.reap_host(q, source, control).await;
    }
    async fn reap_host(
        &self,
        mut q: QuicRecords,
        mut source: frd::media::CaptureSource,
        control: frd::media::ObservationControl,
    ) {
        let cleanup = &self.cleanup;
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
}

async fn scenario(rt: &asupersync::runtime::Runtime, native: &Desktop, end: End) {
    let c = rt.request_cx_with_budget(Budget::INFINITE);
    let h = rt.request_cx_with_budget(Budget::INFINITE);
    let cleanup = rt.request_cx_with_budget(Budget::INFINITE);
    let image = image();
    let (mut app, log) = application(&native.display, &image, 71);
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
    let peer = ClosingPeer {
        h,
        cleanup: cleanup.clone(),
        image,
        log: log.clone(),
        frames_at_close: std::cell::Cell::new(0),
        stop: stop.clone(),
    };
    let (result, ()) = Box::pin(support::both(
        app.run(1, viewer),
        Box::pin(peer.run(q, routes, native, end)),
    ))
    .await;
    assert!(stop.is_stopped());
    match end {
        End::Report(report) => {
            assert_eq!(result, Ok(()));
            let outcome = app.disconnect_outcome().unwrap();
            assert_eq!(outcome.report, Some(report));
            assert_eq!(outcome.transport, Ok(()));
        }
        End::Silent => {
            assert_eq!(result, Ok(()));
            let outcome = app.disconnect_outcome().unwrap();
            assert!(outcome.request_acknowledged);
            assert_eq!(outcome.report, None);
            assert!(outcome.transport.is_err());
        }
        End::Emergency | End::Hidden => {
            assert!(result.is_err());
            assert!(app.disconnect_outcome().is_none());
        }
    }
    assert_eq!(
        log.borrow().frames,
        peer.frames_at_close.get(),
        "post-close UI callback"
    );
    let outcome = app.disconnect_outcome();
    let old = log.borrow().windows[0].clone();
    let deadline = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
    assert_eq!(app.cleanup(&cleanup, deadline).await, Ok(()));
    assert!(app.desktop().is_none());
    assert_eq!(app.cleanup_failure(), None);
    assert_eq!(
        app.disconnect_outcome(),
        outcome,
        "cleanup erased remote outcome"
    );
    assert!(matches!(
        old.status(),
        fr_native::viewer_window::Status::Stopped(_)
    ));
    assert!(matches!(
        app.last_cleanup().unwrap().window,
        WindowCleanup::Complete(_)
    ));
    assert_eq!(log.borrow().ready, [1]);
}
#[test]
fn wm_close_keeps_the_original_connection_until_exact_host_report_and_native_reap() {
    let rt = support::runtime();
    let native = Desktop::start();
    rt.block_on(Box::pin(scenario(&rt, &native, End::Report(report()))));
}
#[test]
fn reported_cleanup_and_pending_effects_remain_separate_from_local_cleanup() {
    let rt = support::runtime();
    let native = Desktop::start();
    rt.block_on(Box::pin(scenario(
        &rt,
        &native,
        End::Report(Closed {
            cleanup: Cleanup::Complete,
            effects: OutstandingEffects::Known {
                pending: 2,
                uncertain: 7,
            },
            ..report()
        }),
    )));
}
#[test]
fn missing_host_report_is_retained_as_unknown_after_normal_window_close() {
    let rt = support::runtime();
    let native = Desktop::start();
    rt.block_on(Box::pin(scenario(&rt, &native, End::Silent)));
}
#[test]
fn emergency_stop_and_native_hide_do_not_masquerade_as_orderly_close() {
    for end in [End::Emergency, End::Hidden] {
        let rt = support::runtime();
        let native = Desktop::start();
        rt.block_on(Box::pin(scenario(&rt, &native, end)));
    }
}
