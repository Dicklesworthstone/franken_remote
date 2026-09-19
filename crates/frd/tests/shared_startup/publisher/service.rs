//! Actual independent network turns beside the continuously owned capture loop.
use super::*;
use asupersync::time::sleep;

async fn both<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let mut a = pin!(a);
    let mut b = pin!(b);
    let mut first = None;
    let mut second = None;
    poll_fn(|task| {
        if first.is_none()
            && let Poll::Ready(value) = a.as_mut().poll(task)
        {
            first = Some(value);
        }
        if second.is_none()
            && let Poll::Ready(value) = b.as_mut().poll(task)
        {
            second = Some(value);
        }
        if first.is_some() && second.is_some() {
            Poll::Ready((first.take().unwrap(), second.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}

// Link::drive may complete synchronously while datagrams remain ready. Yield
// between BOUNDED network turns so the joined source future is actually polled;
// this models the independent connection tasks required by Publisher::serve.
async fn receive_frame(peer: &mut Peer, cx: &Cx, frame: u64) {
    let until = clock(cx) + 1_000_000;
    loop {
        assert!(clock(cx) < until, "continuous frame did not reach decoder");
        peer.service(cx, 1).unwrap();
        peer.link.drive(cx).await;
        peer.media
            .receive_ready(
                cx,
                &mut peer.link.c,
                || true,
                |channel, bytes| {
                    peer.receiver.receive(channel, bytes, clock(cx)).unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some(receipt) = peer
            .presenter
            .present_next(cx, &mut peer.receiver)
            .await
            .unwrap()
        {
            assert_eq!(receipt.frame.as_raw(), frame);
            return;
        }
        sleep(cx.now(), Duration::from_millis(1)).await;
    }
}

#[test]
fn source_service_keeps_capturing_for_the_other_viewer_after_first_departure() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let pid = cohort.publisher.worker_id();
        let mut reports = Vec::new();
        let [first, second] = cohort.peers.as_mut_slice() else {
            panic!("two peers")
        };
        let source = cohort
            .publisher
            .serve(Duration::from_millis(100), |report| reports.push(report));
        sendable(&source);
        let first_viewer = async {
            for frame in 1..=2 {
                Box::pin(receive_frame(first, &cx, frame)).await;
            }
            drop(first.subscriber.take());
        };
        let second_viewer = async {
            for frame in 1..=4 {
                Box::pin(receive_frame(second, &cx, frame)).await;
            }
            drop(second.subscriber.take());
        };
        let (result, _) = Box::pin(both(source, both(first_viewer, second_viewer))).await;
        assert_eq!(result, Err(PublishError::Closed));
        assert_eq!(
            reports.iter().map(|r| r.frame).collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(
            reports.iter().map(|r| r.delivered).collect::<Vec<_>>(),
            [2, 2, 1, 1]
        );
        assert!(reports.iter().all(|r| r.refused == 0));
        assert_eq!(cohort.publisher.worker_id(), pid);
        assert!(cohort.owner.check().is_err());
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn full_cohort_pauses_native_production_instead_of_looping_or_skipping_references() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let mut reports = Vec::new();
        let source = cohort
            .publisher
            .serve(Duration::from_millis(40), |report| reports.push(report));
        let network_pause = async {
            // No UDP/packet service: leave the first dependent picture pending.
            // Stay within its 250ms usefulness bound; no assertion relaxes it.
            sleep(cx.now(), Duration::from_millis(180)).await;
            for peer in &mut cohort.peers {
                drop(peer.subscriber.take());
            }
        };
        assert_eq!(
            Box::pin(both(source, network_pause)).await.0,
            Err(PublishError::Closed)
        );
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].frame, 1);
        assert_eq!(reports[0].delivered, 2);
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn slow_native_completion_does_not_trigger_catchup_captures() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut cohort = Box::pin(joined(&rt, true, true)).await;
        let mut times = Vec::new();
        let source = cohort
            .publisher
            .serve(Duration::from_millis(50), |_| times.push(clock(&cx)));
        let [first, second] = cohort.peers.as_mut_slice() else {
            panic!("two peers")
        };
        let network = both(
            async {
                for frame in 1..=3 {
                    Box::pin(receive_frame(first, &cx, frame)).await;
                }
                drop(first.subscriber.take());
            },
            async {
                for frame in 1..=3 {
                    Box::pin(receive_frame(second, &cx, frame)).await;
                }
                drop(second.subscriber.take());
            },
        );
        assert_eq!(
            Box::pin(both(source, network)).await.0,
            Err(PublishError::Closed)
        );
        assert_eq!(times.len(), 3);
        // The exact native fixture sleeps 50ms. The configured interval follows
        // completion instead of issuing two overdue operations back-to-back.
        assert!(times.windows(2).all(|w| w[1] - w[0] >= 100_000));
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn idle_source_service_observes_revocation_before_its_next_one_second_capture() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let mut reports = 0;
        let source = cohort
            .publisher
            .serve(Duration::from_secs(1), |_| reports += 1);
        let revoke = async {
            sleep(cx.now(), Duration::from_millis(20)).await;
            cohort.owner.revoke();
        };
        let start = clock(&cx);
        let result = Box::pin(both(source, revoke)).await.0;
        assert!(result.is_err());
        assert_eq!(reports, 0);
        assert!(
            clock(&cx) - start < 500_000,
            "idle source failed to service consent"
        );
        assert!(cohort.peers.iter().all(|p| p.control.check().is_err()));
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn cancelling_service_before_poll_or_during_idle_sleep_fences_all_subscribers() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        for poll_once in [false, true] {
            let mut cohort = Box::pin(joined(&rt, true, false)).await;
            let mut reports = 0;
            {
                let mut service = pin!(
                    cohort
                        .publisher
                        .serve(Duration::from_secs(1), |_| reports += 1)
                );
                if poll_once {
                    poll_fn(|task| match service.as_mut().poll(task) {
                        Poll::Pending => Poll::Ready(()),
                        Poll::Ready(result) => panic!("expected idle wait, got {result:?}"),
                    })
                    .await;
                }
            }
            assert_eq!(reports, 0);
            assert!(cohort.owner.check().is_err());
            assert!(cohort.peers.iter().all(|p| p.control.check().is_err()));
            stop_cohort(&mut cohort, &cx).await;
        }
    });
}

#[test]
fn invalid_source_cadence_refuses_without_native_capture_and_ends_the_attempt() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        for interval in [
            Duration::ZERO,
            Duration::from_micros(33_333),
            Duration::from_secs(2),
        ] {
            let mut cohort = Box::pin(joined(&rt, true, false)).await;
            let mut reports = 0;
            assert_eq!(
                cohort.publisher.serve(interval, |_| reports += 1).await,
                Err(PublishError::InvalidBudget)
            );
            assert_eq!(reports, 0);
            assert!(cohort.owner.check().is_err());
            assert!(cohort.peers.iter().all(|p| p.control.check().is_err()));
            stop_cohort(&mut cohort, &cx).await;
        }
    });
}
