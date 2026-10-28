//! Real TLS/UDP, X11 window and supervised child ownership; codec replies are
//! the existing explicit fixture, not decoder/hardware or tailnet qualification.
use super::*;
use frd::{
    media_egress::{Lane, Progress},
    session_startup::{StreamingViewerControl, StreamingViewerError},
};

fn recovery_offer() -> Offer {
    let mut offer = offered();
    offer.capabilities.push(Capability {
        name: fr_wire::recovery_request::CAPABILITY.into(),
        version: fr_wire::recovery_request::VERSION,
        required: true,
    });
    offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    offer
}
fn worker_deadline() -> ObserverError {
    ObserverError::Streaming(StreamingViewerError::Media(frd::media::Error::Worker(
        frd::worker::Error::Deadline,
    )))
}
struct StreamPeer<'a> {
    image: &'a Path,
    log: Rc<RefCell<StateLog>>,
    attempt: u8,
}
impl StreamPeer<'_> {
    async fn run(
        &self,
        q: QuicRecords,
        h: &Cx,
        cleanup: &Cx,
        routes: ControlRoutes,
        stop: &StreamingViewerControl,
    ) {
        let StartedMedia {
            mut transport,
            mut source,
            control,
            mut sender,
            ..
        } = Box::pin(start_stream(
            q,
            h,
            routes,
            self.image,
            Some(stop.clone()),
            recovery_offer(),
        ))
        .await;
        // Startup already completed a real IPC reply. Only the subsequent
        // picture hangs in epoch 700; every fresh epoch must answer it.
        sender
            .enqueue_capture(source.capture_if_changed(&control, true).await.unwrap())
            .unwrap();
        loop {
            if sender.transmit(h, &mut transport, Lane::Original).unwrap() == Progress::Idle {
                break;
            }
            transport
                .drive(h, Duration::from_millis(1), || true)
                .await
                .unwrap();
        }
        // Keep the original socket serviced through the native ceiling. In
        // particular, an acknowledged recovery report must not turn this test
        // into a link-failure retry. No replacement handshake is fabricated.
        while !stop.is_stopped() {
            if self.log.borrow().presented.contains(&(self.attempt, 1)) {
                stop.stop();
                break;
            }
            if let Err(error) = transport.drive(h, Duration::from_millis(1), || true).await {
                assert!(
                    stop.is_stopped(),
                    "premature peer transport failure: {error:?}"
                );
                break;
            }
        }
        transport.close();
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
async fn streamed_attempt(
    runtime: &asupersync::runtime::Runtime,
    session: &mut Session<Interface>,
    log: Rc<RefCell<StateLog>>,
    attempt: u8,
    image: &Path,
) -> Result<(), ObserverError> {
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
    let (client, host) = support::native_pair(&c, "localhost", ALPN).await;
    let viewer = Viewer::new(
        c.clone(),
        client.unwrap(),
        recovery_offer(),
        Policy::default(),
        Duration::from_secs(2),
    )
    .unwrap();
    let stop = viewer.control();
    let (q, routes) = QuicRecords::bootstrap(host.unwrap(), &h, Policy::default()).unwrap();
    let peer = StreamPeer {
        image,
        log,
        attempt,
    };
    let (result, ()) = asupersync::time::timeout(
        c.timer_driver().unwrap().now(),
        Duration::from_secs(8),
        Box::pin(support::both(
            session.run(attempt, viewer),
            Box::pin(peer.run(q, &h, &cleanup, routes, &stop)),
        )),
    )
    .await
    .expect("bounded streamed worker timeout/replacement");
    assert!(stop.is_stopped());
    result
}
fn collect(runtime: &asupersync::runtime::Runtime, session: &mut Session<Interface>) {
    let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
    let deadline = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
    assert_eq!(
        runtime.block_on(session.cleanup(&cleanup, deadline)),
        Ok(())
    );
    assert!(session.desktop().is_none());
    assert_eq!(session.cleanup_failure(), None);
    let report = session.last_cleanup().unwrap();
    assert!(matches!(report.media, Ok(Some(_))));
    assert!(matches!(report.window, WindowCleanup::Complete(_)));
    assert_eq!(
        report.input,
        frd::session_startup::viewer_events::CaptureCleanup::NotStarted
    );
}

#[test]
fn a_hung_stream_worker_is_reaped_before_a_fresh_epoch_can_present_again() {
    let native = Desktop::start();
    let runtime = support::runtime();
    let image = decoder_image("stall-stream-700");
    let (mut session, log) = application(&native.display, &image, 700);
    let started = std::time::Instant::now();
    let first = runtime.block_on(Box::pin(streamed_attempt(
        &runtime,
        &mut session,
        log.clone(),
        1,
        &image,
    )));
    assert_eq!(first, Err(worker_deadline()));
    // Positive recovery negotiation drains the borrow until the unchanged
    // two-second native ceiling, not the picture's 250 ms reference horizon.
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert!(retryable(Failure::Observation(first.unwrap_err())));
    assert_eq!(log.borrow().ready, [1]);
    assert!(!log.borrow().presented.contains(&(1, 1)));
    let old_window = log.borrow().windows[0].clone();
    let old_worker = log.borrow().workers[0];
    assert!(
        session.desktop().is_some(),
        "timeout must retain cleanup custody"
    );
    assert_eq!(session.offer(2, &recovery_offer()), Err(CallbackError));
    let premature = viewer(&runtime);
    let premature_stop = premature.control();
    assert_eq!(
        runtime.block_on(session.run(2, premature)),
        Err(ObserverError::Order)
    );
    assert!(premature_stop.is_stopped());
    assert_eq!(session.desktop().unwrap().worker_id(), Some(old_worker));

    collect(&runtime, &mut session);
    assert!(matches!(
        session.last_cleanup().unwrap().media,
        Ok(Some(status)) if !status.success()
    ));
    assert!(matches!(
        old_window.status(),
        fr_native::viewer_window::Status::Stopped(_)
    ));
    session.offer(2, &recovery_offer()).unwrap();
    let _ended_after_frame = runtime.block_on(Box::pin(streamed_attempt(
        &runtime,
        &mut session,
        log.clone(),
        2,
        &image,
    )));
    assert_eq!(log.borrow().ready, [1, 2]);
    assert!(log.borrow().presented.contains(&(2, 1)));
    assert_ne!(log.borrow().workers[1], old_worker);
    assert!(session.desktop().unwrap().input().is_none());
    collect(&runtime, &mut session);
}
