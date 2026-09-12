//! Real native UDP/TLS, with deliberately controlled admission-refresh futures.
//! No synthetic identity evidence is installed through the public constructor.
use super::*;
use crate::session_startup::admission_refresh::Bound;
use asupersync::time::{sleep, timeout};

fn assert_send<T: Send>(_: &T) {}
async fn pending_once<F: Future>(mut future: std::pin::Pin<&mut F>) {
    std::future::poll_fn(|task| {
        assert!(future.as_mut().poll(task).is_pending());
        Poll::Ready(())
    })
    .await;
}
struct Unfinished {
    alive: Arc<AtomicBool>,
    decision: Arc<AtomicU8>,
    dropped: Arc<AtomicBool>,
}
impl Future for Unfinished {
    type Output = Result<(), Error>;
    fn poll(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}
impl Drop for Unfinished {
    fn drop(&mut self) {
        assert!(
            !self.alive.load(Ordering::Acquire),
            "lookup dropped before peer revocation"
        );
        assert_eq!(self.decision.load(Ordering::Acquire), RETIRED);
        self.dropped.store(true, Ordering::Release);
    }
}

#[test]
fn lookup_wait_keeps_actual_queued_capabilities_deliverable_without_consent() {
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut host, mut viewer, alive) = pair(&c, &h, config(true), true).await;
        viewer.step(&c);
        while host.phase == Phase::Hello {
            let (a, b) = Box::pin(support::both(
                host.transport
                    .as_mut()
                    .unwrap()
                    .drive(&h, Duration::from_millis(1), || true),
                viewer.quic.drive(&c, Duration::from_millis(1), || true),
            ))
            .await;
            a.unwrap();
            b.unwrap();
            host.tick().unwrap();
        }
        assert_eq!(host.phase, Phase::Selection);
        assert_ne!(host.len, 0, "capability reply was not staged");
        host.tick().unwrap();
        assert_eq!(host.len, 0);
        let binding = host.transport.as_ref().unwrap().binding();
        let until = host.peer.as_ref().unwrap().check(&h, host.role).unwrap();
        let bound = Bound::new(&host, until).unwrap();
        let received = Arc::new(AtomicBool::new(false));
        let lookup_seen = received.clone();
        let completed = Arc::new(AtomicBool::new(false));
        let finished = completed.clone();
        let lookup = async {
            sleep(h.now(), Duration::from_millis(80)).await;
            assert!(
                lookup_seen.load(Ordering::Acquire),
                "queued reply stalled behind LocalAPI"
            );
            completed.store(true, Ordering::Release);
            Ok(())
        };
        let pump = bound.run(
            host.transport.as_mut().unwrap(),
            lookup,
            Duration::from_millis(2),
        );
        assert_send(&pump);
        let client = async {
            while !received.load(Ordering::Acquire) {
                viewer
                    .quic
                    .drive(&c, Duration::from_millis(2), || true)
                    .await
                    .unwrap();
                viewer
                    .quic
                    .receive_ready(
                        &c,
                        || true,
                        |_| true,
                        |route, bytes| {
                            assert_eq!(route, Route::Stream(viewer.routes.inbound));
                            assert!(matches!(
                                negotiation::decode(bytes, 4096, 0).unwrap(),
                                Message::HostCapabilities(_)
                            ));
                            assert!(!finished.load(Ordering::Acquire));
                            viewer.startup.receive(bytes, now(&c).unwrap()).unwrap();
                            received.store(true, Ordering::Release);
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
            }
        };
        let (result, ()) = timeout(
            c.now(),
            Duration::from_secs(1),
            Box::pin(support::both(pump, client)),
        )
        .await
        .unwrap();
        result.unwrap();
        assert!(alive.load(Ordering::Acquire));
        assert!(host.observation_until.is_none());
        assert!(host.transport.as_ref().unwrap().is_bound_to(&binding));
        Box::pin(ready(&c, &mut host, &mut viewer, true)).await;
        host.finish()
            .unwrap()
            .observation()
            .unwrap()
            .check()
            .unwrap();
    });
}

#[test]
fn successful_refresh_finishes_its_pending_native_turn_without_cancelling_connection() {
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut host, mut viewer, alive) = pair(&c, &h, config(false), true).await;
        let bound = Bound::new(
            &host,
            host.peer.as_ref().unwrap().check(&h, host.role).unwrap(),
        )
        .unwrap();
        let mut polled = false;
        let lookup = std::future::poll_fn(|task| {
            if polled {
                Poll::Ready(Ok(()))
            } else {
                polled = true;
                task.waker().wake_by_ref();
                Poll::Pending
            }
        });
        bound
            .run(
                host.transport.as_mut().unwrap(),
                lookup,
                Duration::from_millis(5),
            )
            .await
            .unwrap();
        assert!(!host.transport.as_ref().unwrap().is_closed());
        assert!(alive.load(Ordering::Acquire));
        Box::pin(ready(&c, &mut host, &mut viewer, false)).await;
    });
}

#[test]
fn denial_or_parent_cancellation_fences_before_dropping_pending_refresh() {
    for denied in [false, true] {
        let runtime = support::runtime();
        let c = runtime.request_cx_with_budget(Budget::INFINITE);
        let h = runtime.request_cx_with_budget(Budget::INFINITE);
        runtime.block_on(async {
            let (mut host, mut viewer, alive) = pair(&c, &h, config(true), true).await;
            while host.approval().is_none() {
                let (a, ()) = Box::pin(support::both(
                    host.drive(Duration::from_millis(1)),
                    viewer.drive(&c),
                ))
                .await;
                a.unwrap();
            }
            let approval = host.approval().unwrap();
            let bound = Bound::new(
                &host,
                host.peer.as_ref().unwrap().check(&h, host.role).unwrap(),
            )
            .unwrap();
            let dropped = Arc::new(AtomicBool::new(false));
            let lookup = Unfinished {
                alive: alive.clone(),
                decision: host.approval.clone(),
                dropped: dropped.clone(),
            };
            {
                let mut pump = pin!(bound.run(
                    host.transport.as_mut().unwrap(),
                    lookup,
                    Duration::from_millis(100)
                ));
                pending_once(pump.as_mut()).await;
                if denied {
                    approval.decide(false).unwrap();
                } else {
                    h.cancel_fast(asupersync::types::CancelKind::User);
                }
                assert_eq!(
                    pump.await,
                    Err(if denied {
                        Error::Denied
                    } else {
                        Error::Cancelled
                    })
                );
            }
            assert!(dropped.load(Ordering::Acquire));
            assert!(approval.decide(true).is_err());
            assert!(host.observation_until.is_none());
            assert!(c.checkpoint().is_ok());
        });
    }
}

#[test]
fn expiry_fences_peer_before_abandoning_silent_lookup_and_preserves_initial_deadlines() {
    for peer_first in [false, true] {
        let runtime = support::runtime();
        let c = runtime.request_cx_with_budget(Budget::INFINITE);
        let h = runtime.request_cx_with_budget(Budget::INFINITE);
        runtime.block_on(async {
            let (mut host, _viewer, alive) = pair(&c, &h, config(true), true).await;
            let deadline = now(&h).unwrap() + 15_000;
            if peer_first {
                let Peer::Fixture { until, .. } = host.peer.as_mut().unwrap() else {
                    unreachable!()
                };
                *until = deadline;
            } else {
                host.until = deadline;
            }
            let old = host.until;
            let bound = Bound::new(
                &host,
                host.peer.as_ref().unwrap().check(&h, host.role).unwrap(),
            )
            .unwrap();
            let dropped = Arc::new(AtomicBool::new(false));
            let lookup = Unfinished {
                alive: alive.clone(),
                decision: host.approval.clone(),
                dropped: dropped.clone(),
            };
            assert_eq!(
                bound
                    .run(host.transport.as_mut().unwrap(), lookup, Duration::ZERO)
                    .await,
                Err(Error::Expired)
            );
            assert!(dropped.load(Ordering::Acquire));
            assert_eq!(host.until, old);
            assert!(host.observation_until.is_none());
        });
    }
}

#[test]
fn late_ready_refresh_cannot_cross_the_original_proof_expiry() {
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut host, _viewer, alive) = pair(&c, &h, config(false), true).await;
        let deadline = now(&h).unwrap() + 10_000;
        let Peer::Fixture { until, .. } = host.peer.as_mut().unwrap() else {
            unreachable!()
        };
        *until = deadline;
        let bound = Bound::new(&host, deadline).unwrap();
        let lookup = std::future::poll_fn(|_| {
            // Deliberately advances time inside one poll, like a delayed callback.
            std::thread::sleep(Duration::from_millis(20));
            Poll::Ready(Ok(()))
        });
        assert_eq!(
            bound
                .run(
                    host.transport.as_mut().unwrap(),
                    lookup,
                    Duration::from_millis(2)
                )
                .await,
            Err(Error::Expired)
        );
        assert!(!alive.load(Ordering::Acquire));
        assert!(host.finish().is_err());
    });
}

#[test]
fn unpolled_refresh_and_public_startup_drive_are_terminal_without_running_lookup() {
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut host, _viewer, alive) = pair(&c, &h, config(true), true).await;
        let bound = Bound::new(
            &host,
            host.peer.as_ref().unwrap().check(&h, host.role).unwrap(),
        )
        .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let lookup = Unfinished {
            alive: alive.clone(),
            decision: host.approval.clone(),
            dropped: dropped.clone(),
        };
        drop(bound.run(
            host.transport.as_mut().unwrap(),
            lookup,
            Duration::from_millis(2),
        ));
        assert!(dropped.load(Ordering::Acquire));
        assert!(host.finish().is_err());
        let (mut host, _viewer, alive) = pair(&c, &h, config(true), true).await;
        let drive = host.drive(Duration::from_millis(10));
        assert_send(&drive);
        drop(drive);
        assert!(!alive.load(Ordering::Acquire));
        assert_eq!(host.phase, Phase::Closed);
        assert!(host.transport.as_ref().unwrap().is_closed());
    });
}
