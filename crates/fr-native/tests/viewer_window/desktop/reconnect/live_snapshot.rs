//! Real Desktop serving, TLS/UDP and a confined native presenter. The host
//! replays the explicit HEVC capture fixture; this is not Tailscale discovery,
//! source-freshness, hardware or physical-display qualification.
use super::*;
use fr_client::robot::snapshot::{Error as SnapshotError, Handle, Image, Request};
use std::{future::poll_fn, pin::pin};

#[derive(Default)]
struct LiveState {
    handle: Option<Handle>,
    pixels: Option<Vec<u8>>,
    ready: usize,
    callbacks: usize,
    workers: Vec<u32>,
}
struct LiveUi<'a> {
    native: &'a Desktop,
    state: Rc<RefCell<LiveState>>,
}
impl Ui for LiveUi<'_> {
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
        let window = desktop.window().unwrap().target().unwrap().window();
        let mut state = self.state.borrow_mut();
        assert!(state.handle.is_none(), "one original viewer only");
        state.handle = Some(desktop.snapshot_handle().unwrap());
        state.pixels = Some(self.native.dump(window, 160, 120));
        state.workers.push(desktop.worker_id().unwrap());
        state.ready += 1;
        Ok(())
    }
    fn frame(
        &mut self,
        _: u8,
        _: Option<frd::session_startup::Presentation>,
    ) -> Result<(), CallbackError> {
        self.state.borrow_mut().callbacks += 1;
        Ok(())
    }
}

async fn read_live(request: Request, q: &mut QuicRecords, host: &Cx) -> Image {
    let mut request = pin!(request);
    loop {
        let mut completed = None;
        let mut turn = pin!(q.drive(host, Duration::from_millis(1), || true));
        // Keep the original host socket progressing while its existing viewer
        // services the mailbox. Neither future is dropped midway through I/O.
        poll_fn(|task| {
            if completed.is_none()
                && let Poll::Ready(result) = request.as_mut().poll(task)
            {
                completed = Some(result);
            }
            match turn.as_mut().poll(task) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => {
                    result.unwrap();
                    Poll::Ready(())
                }
            }
        })
        .await;
        if let Some(result) = completed {
            return result.expect("live screenshot under its original deadline");
        }
    }
}

async fn live_peer(
    q: QuicRecords,
    host: &Cx,
    cleanup: &Cx,
    routes: ControlRoutes,
    capture: &Path,
    stopped: frd::session_startup::StreamingViewerControl,
    state: &Rc<RefCell<LiveState>>,
) {
    let (mut q, mut source, control, _, _) =
        Box::pin(start_media(q, host, routes, capture, Some(stopped.clone()))).await;
    let until = support::clock(host) + 2_000_000;
    let handle = loop {
        if let Some(handle) = state.borrow().handle.clone() {
            break handle;
        }
        assert!(
            support::clock(host) < until,
            "ready callback did not publish handle"
        );
        q.drive(host, Duration::from_millis(1), || true)
            .await
            .unwrap();
    };
    let stamp = handle.current_stamp().expect("actual initial presentation");
    assert!(!stopped.is_stopped());
    let first = read_live(handle.request(stamp).unwrap(), &mut q, host).await;
    assert_eq!(first.snapshot().stamp(), stamp);
    assert_eq!(
        (first.snapshot().width(), first.snapshot().height()),
        (160, 120)
    );
    assert_eq!(
        first.snapshot().pixels(),
        state.borrow().pixels.as_ref().unwrap()
    );
    assert_eq!(first.check(), Ok(()));
    assert_eq!(handle.request(stamp).err(), Some(SnapshotError::Busy));
    drop(first);

    // Cancelling only a local request does not stop the original observation,
    // replace its native owner, or require another frame on this static desktop.
    let abandoned = handle.request(stamp).unwrap();
    drop(abandoned);
    assert!(!stopped.is_stopped());
    let second = read_live(handle.request(stamp).unwrap(), &mut q, host).await;
    assert_eq!(second.snapshot().stamp(), stamp);
    assert_eq!(
        second.snapshot().pixels(),
        state.borrow().pixels.as_ref().unwrap()
    );
    assert_eq!(state.borrow().ready, 1);
    assert_eq!(state.borrow().workers.len(), 1);
    assert!(state.borrow().callbacks > 0);
    assert!(!stopped.is_stopped());

    // Retirement is synchronous even before the runtime polls the serving
    // future again: a delivered image cannot outlive the original stop decision.
    stopped.stop();
    assert_eq!(second.check(), Err(SnapshotError::Closed));
    assert!(handle.current_stamp().is_none());
    assert_eq!(handle.request(stamp).err(), Some(SnapshotError::Closed));
    drop(second);
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
fn live_requests_reuse_the_original_desktop_and_stop_retires_delivered_pixels() {
    let native = Desktop::start();
    let runtime = support::runtime();
    let client = runtime.request_cx_with_budget(Budget::INFINITE);
    let host = runtime.request_cx_with_budget(Budget::INFINITE);
    let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
    let capture = image();
    let state = Rc::new(RefCell::new(LiveState::default()));
    let configuration = Configuration::new(
        Path::new(env!("CARGO_BIN_EXE_fr-media-worker")),
        &native.display,
        None,
        1002,
    )
    .unwrap()
    .with_fitted_window(160, 120)
    .unwrap();
    let mut session = Session::new(
        configuration,
        ObserverPolicy {
            timeout: Duration::from_secs(2),
            ..ObserverPolicy::default()
        },
        Mode::Observe,
        LiveUi {
            native: &native,
            state: state.clone(),
        },
    );
    runtime.block_on(async {
        let (client_transport, host_transport) =
            support::native_pair(&client, "localhost", ALPN).await;
        let viewer = Viewer::new(
            client.clone(),
            client_transport.unwrap(),
            offered(),
            Policy::default(),
            Duration::from_secs(2),
        )
        .unwrap();
        let stopped = viewer.control();
        let (q, routes) =
            QuicRecords::bootstrap(host_transport.unwrap(), &host, Policy::default()).unwrap();
        let (result, ()) = asupersync::time::timeout(
            client.now(),
            Duration::from_secs(8),
            Box::pin(support::both(
                session.run(1, viewer),
                live_peer(
                    q,
                    &host,
                    &cleanup,
                    routes,
                    &capture,
                    stopped.clone(),
                    &state,
                ),
            )),
        )
        .await
        .expect("bounded original live screenshot lifecycle");
        assert!(
            result.is_err(),
            "explicit terminal stop is not a completed observation"
        );
        assert!(stopped.is_stopped());
        assert_eq!(
            session
                .cleanup(
                    &cleanup,
                    Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
                )
                .await,
            Ok(())
        );
        assert!(session.desktop().is_none());
        assert_eq!(session.cleanup_failure(), None);
        let report = session.last_cleanup().unwrap();
        assert!(matches!(report.media, Ok(Some(_))));
        assert!(matches!(report.window, WindowCleanup::Complete(_)));
        assert!(
            state
                .borrow()
                .handle
                .as_ref()
                .unwrap()
                .current_stamp()
                .is_none()
        );
        assert!(
            session.take_initial_snapshot().is_none(),
            "no automatic image retention"
        );
    });
}
