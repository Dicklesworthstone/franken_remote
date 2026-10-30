//! Actual TLS/UDP startup, confined native HEVC decoder and independent X11
//! readback. The host replays an explicit HEVC capture fixture; this does not
//! qualify Tailscale discovery, capture freshness or physical display visibility.
use super::*;
use fr_wire::{
    authority,
    closure::{self, Cleanup, Closed, ClosedReason, OutstandingEffects},
};

struct SnapshotUi<'a> {
    native: &'a Desktop,
    size: (u32, u32),
    pixels: Rc<RefCell<Option<Vec<u8>>>>,
}
impl Ui for SnapshotUi<'_> {
    fn choose(&mut self, _: u8, _: &display::Catalog) -> Result<Option<u128>, CallbackError> {
        Ok(Some(9))
    }
    fn approval(
        &mut self,
        _: u8,
        _: fr_client::startup::ApprovalNotice,
    ) -> Result<(), CallbackError> {
        panic!("explicit peer has no approval notification")
    }
    fn ready(&mut self, _: u8, desktop: &mut ClientDesktop) -> Result<(), CallbackError> {
        assert!(desktop.input().is_none());
        let id = desktop.window().unwrap().target().unwrap().window();
        // Blocking independent peer readback is test instrumentation only.
        *self.pixels.borrow_mut() = Some(self.native.dump(id, self.size.0, self.size.1));
        Ok(())
    }
    fn frame(
        &mut self,
        _: u8,
        _: Option<frd::session_startup::Presentation>,
    ) -> Result<(), CallbackError> {
        panic!("one-shot observation must close before continuous serving")
    }
}

async fn snapshot_peer(q: QuicRecords, h: &Cx, cleanup: &Cx, routes: ControlRoutes, image: &Path) {
    let (mut q, mut source, control, parent, routes) =
        Box::pin(start_media(q, h, routes, image, None)).await;
    let binding = authority::Binding {
        channel: parent.id,
        session: parent.remote_session,
    };
    let mut requested = false;
    let until = support::clock(h) + 2_000_000;
    while !requested {
        assert!(support::clock(h) < until, "one-shot close did not arrive");
        q.drive(h, Duration::from_millis(1), || true).await.unwrap();
        q.receive_ready(
            h,
            || true,
            |route| route == Route::Stream(routes.inbound),
            |_, bytes| {
                let value = closure::decode_request(
                    bytes,
                    binding,
                    &ProtocolLimits::ABSOLUTE,
                    InputDirection::ViewerToHost,
                    InputDelivery::Reliable,
                )
                .unwrap();
                assert_eq!(value.reason, closure::Reason::InspectionComplete);
                assert!(!requested, "one-shot close was repeated");
                requested = true;
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
    }
    control.revoke();
    source.worker_mut().abort();
    let original = q.binding();
    q.close_with_closed(
        cleanup,
        &original,
        routes.outbound,
        binding,
        Closed {
            reason: ClosedReason::ClientRequested,
            cleanup: Cleanup::Unconfirmed,
            effects: OutstandingEffects::Unknown,
        },
    )
    .await
    .unwrap();
    q.close();
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
fn one_shot_snapshot_matches_actual_native_and_fitted_windows_after_original_session_cleanup() {
    for size in [(320, 240), (160, 120)] {
        let native = Desktop::start();
        let runtime = support::runtime();
        let c = runtime.request_cx_with_budget(Budget::INFINITE);
        let h = runtime.request_cx_with_budget(Budget::INFINITE);
        let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
        let capture = image();
        let pixels = Rc::new(RefCell::new(None));
        let mut configuration = Configuration::new(
            Path::new(env!("CARGO_BIN_EXE_fr-media-worker")),
            &native.display,
            None,
            1001,
        )
        .unwrap()
        .with_initial_snapshot();
        if size != (320, 240) {
            configuration = configuration.with_fitted_window(size.0, size.1).unwrap();
        }
        let mut session = Session::observing_once(
            configuration,
            ObserverPolicy {
                timeout: Duration::from_secs(2),
                ..ObserverPolicy::default()
            },
            SnapshotUi {
                native: &native,
                size,
                pixels: pixels.clone(),
            },
        );
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
                c.now(),
                Duration::from_secs(8),
                Box::pin(support::both(
                    session.run(1, viewer),
                    snapshot_peer(q, &h, &cleanup, routes, &capture),
                )),
            )
            .await
            .expect("bounded native screenshot lifecycle");
            assert_eq!(result, Ok(()));
            assert!(stop.is_stopped());
            let initial = session.initial_observation().unwrap();
            assert_eq!(
                initial.completion.stage,
                frd::media::PresentationStage::SubmittedToCompositor
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
            assert!(session.desktop().is_none());
            assert_eq!(session.cleanup_failure(), None);
            let snapshot = session
                .take_initial_snapshot()
                .expect("explicit real presentation");
            assert_eq!(snapshot.stamp().frame, initial.completion.frame);
            assert_eq!(
                snapshot.stamp().configuration,
                fr_core::ids::CodecConfigurationGeneration::INITIAL
            );
            assert_eq!(
                snapshot.stamp().recovery,
                fr_core::ids::RecoveryGeneration::INITIAL
            );
            assert_eq!((snapshot.width(), snapshot.height()), size);
            let actual = pixels.borrow();
            assert_eq!(snapshot.pixels(), actual.as_ref().unwrap());
            assert!(session.take_initial_snapshot().is_none());
            assert!(matches!(session.last_cleanup().unwrap().media, Ok(Some(_))));
            assert!(matches!(
                session.last_cleanup().unwrap().window,
                WindowCleanup::Complete(_)
            ));
        });
    }
}
