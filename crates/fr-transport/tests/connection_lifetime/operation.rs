//! Optional media retains its original send guards on every connection path.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[test]
fn operation_guard_can_attach_after_traffic_and_cannot_be_replaced() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let bytes = recovery_packet(2, 8);
        p.server
            .send(
                &cx,
                Route::Stream(p.host_routes[1]),
                &bytes,
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        assert_eq!(
            p.server.retain_lifetime_check(&cx, Arc::new(|| true)),
            Err(Error::InvalidPolicy),
            "security evidence still must precede application traffic"
        );
        let live = Arc::new(AtomicBool::new(true));
        let check = live.clone();
        p.server
            .retain_operation_check(&cx, Arc::new(move || check.load(Ordering::Acquire)))
            .unwrap();
        assert_eq!(
            p.server.retain_operation_check(&cx, Arc::new(|| false)),
            Err(Error::InvalidPolicy)
        );
        p.server.tick(&cx, || true).unwrap();
        live.store(false, Ordering::Release);
        assert_eq!(
            p.server.send(
                &cx,
                Route::Stream(p.host_routes[1]),
                &bytes,
                clock(&cx) + 1_000_000,
                || true
            ),
            Err(Error::Unauthorized)
        );
        assert!(p.server.is_closed());
        assert_eq!(p.server.usage().retained_send_records, 0);
    });
}

#[test]
fn operation_guard_fences_receive_dispatch_and_failed_registration() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let live = Arc::new(AtomicBool::new(true));
        let check = live.clone();
        p.client
            .retain_operation_check(&cx, Arc::new(move || check.load(Ordering::Acquire)))
            .unwrap();
        p.server
            .send(
                &cx,
                Route::Stream(p.host_routes[1]),
                &recovery_packet(2, 8),
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        for _ in 0..4 {
            drive(&cx, &mut p).await;
        }
        live.store(false, Ordering::Release);
        assert_eq!(
            p.client.receive(
                &cx,
                || true,
                |_, _| panic!("expired operation guard cannot dispatch")
            ),
            Err(Error::Unauthorized)
        );
        assert!(p.client.is_closed());
        assert_eq!(
            p.server.retain_operation_check(&cx, Arc::new(|| false)),
            Err(Error::Unauthorized)
        );
        assert!(p.server.is_closed());
    });
}

#[test]
fn pending_native_io_rechecks_operation_guard_before_resuming() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let live = Arc::new(AtomicBool::new(true));
        let check = live.clone();
        p.server
            .retain_operation_check(&cx, Arc::new(move || check.load(Ordering::Acquire)))
            .unwrap();
        let mut tested = false;
        for _ in 0..16 {
            let mut io = pin!(p.server.drive(&cx, Duration::from_millis(100), || true));
            let pending =
                std::future::poll_fn(|task| Poll::Ready(io.as_mut().poll(task).is_pending())).await;
            if pending {
                live.store(false, Ordering::Release);
                let result = std::future::poll_fn(|task| Poll::Ready(io.as_mut().poll(task))).await;
                assert_eq!(result, Poll::Ready(Err(Error::Unauthorized)));
                tested = true;
                break;
            }
        }
        assert!(
            tested,
            "must exercise a genuinely pending native I/O future"
        );
        assert!(p.server.is_closed());
    });
}

#[test]
fn operation_guard_never_replaces_original_connection_security() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx, Policy::default()).await;
        let live = Arc::new(AtomicBool::new(true));
        let check = live.clone();
        p.server
            .retain_lifetime_check(&cx, Arc::new(move || check.load(Ordering::Acquire)))
            .unwrap();
        p.server
            .retain_operation_check(&cx, Arc::new(|| true))
            .unwrap();
        live.store(false, Ordering::Release);
        assert_eq!(p.server.tick(&cx, || true), Err(Error::Unauthorized));
        assert!(p.server.is_closed());
    });
}
