//! Original viewer and connection supervision over real localhost TLS/UDP.
//! Destination evidence is an explicit fixture; this is not live-tailnet proof.
use super::*;
use fr_transport::quic::ClientAbortReport;

#[test]
fn viewer_failure_notifies_before_the_supervisor_cancels_its_connection() {
    let runtime = network::runtime();
    let connection = runtime.request_cx_with_budget(Budget::INFINITE);
    let viewing = runtime.request_cx_with_budget(Budget::INFINITE);
    let host_cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(Box::pin(async {
        let (native, host) =
            network::native_pair(&connection, "localhost", fr_transport::quic::ALPN).await;
        let mut host = host.unwrap();
        let destination = connection.clone();
        let mut viewer = join_viewer(
            &viewing,
            native.unwrap(),
            Arc::new(move || destination.checkpoint().is_ok()),
            offer(),
            Configuration::default(),
        )
        .unwrap();
        let abort = ClientAbortReport::default();
        viewer
            .arm_client_abort(&connection, abort.registration())
            .unwrap();
        let serviced_after_fence = AtomicBool::new(false);
        let service = poll_fn(|_| {
            if viewing.is_cancel_requested() && connection.checkpoint().is_ok() {
                serviced_after_fence.store(true, Ordering::Release);
            }
            Poll::<Result<(), fr_tailnet::Error>>::Pending
        });
        let result = run_application(
            &connection,
            service,
            finish_application(
                async {
                    viewer.close();
                    assert!(viewing.is_cancel_requested());
                    Err::<(), _>("original application failure")
                },
                abort,
            ),
        )
        .await;
        assert_eq!(result, Ok(Err("original application failure")));
        assert!(serviced_after_fence.load(Ordering::Acquire));
        assert!(connection.is_cancel_requested());
        let until = network::clock(&host_cx) + 1_000_000;
        while !host.connection().close_was_peer_initiated() {
            assert!(network::clock(&host_cx) < until, "viewer ended silently");
            host.drive_io_once(&host_cx, Duration::from_millis(5))
                .await
                .unwrap();
        }
        assert_eq!(
            host.connection().inner().transport().close_code(),
            Some(0x4652_0301)
        );
    }));
}

#[test]
fn lost_destination_or_shared_cancelled_context_suppresses_notification() {
    for shared_context in [false, true] {
        let runtime = network::runtime();
        let connection = runtime.request_cx_with_budget(Budget::INFINITE);
        let viewing = if shared_context {
            connection.clone()
        } else {
            runtime.request_cx_with_budget(Budget::INFINITE)
        };
        let host_cx = runtime.request_cx_with_budget(Budget::INFINITE);
        runtime.block_on(Box::pin(async {
            let (native, host) =
                network::native_pair(&connection, "localhost", fr_transport::quic::ALPN).await;
            let mut host = host.unwrap();
            let live = Arc::new(AtomicBool::new(true));
            let guard = live.clone();
            let mut viewer = join_viewer(
                &viewing,
                native.unwrap(),
                Arc::new(move || guard.load(Ordering::Acquire)),
                offer(),
                Configuration::default(),
            )
            .unwrap();
            let abort = ClientAbortReport::default();
            viewer
                .arm_client_abort(&connection, abort.registration())
                .unwrap();
            let result = finish_application(
                async {
                    viewer.close();
                    if !shared_context {
                        live.store(false, Ordering::Release);
                    }
                    Err::<(), _>("original application failure")
                },
                abort,
            )
            .await;
            assert_eq!(result, Err("original application failure"));
            assert!(viewing.is_cancel_requested());
            host.drive_io_once(&host_cx, Duration::from_millis(20))
                .await
                .unwrap();
            assert!(!host.connection().close_was_peer_initiated());
        }));
    }
}
