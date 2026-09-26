//! Session-terminal reports through the canonical bounded drain.
use super::*;

// Session closure shares the existing socket custody, deadline and backlog
// checks with revocation. Reports below are owner fixtures, not cleanup proof.
fn closed_report() -> fr_wire::closure::Closed {
    fr_wire::closure::Closed {
        reason: fr_wire::closure::ClosedReason::HostStopping,
        cleanup: fr_wire::closure::Cleanup::Unconfirmed,
        effects: fr_wire::closure::OutstandingEffects::Unknown,
    }
}
async fn collect_terminal(
    cx: &Cx,
    pair: &mut Pair,
    end: impl std::future::Future<Output = Result<(), Error>>,
) -> (Result<(), Error>, Vec<Vec<u8>>) {
    let done = Cell::new(false);
    let mut received = Vec::new();
    let (outcome, ()) = Box::pin(both(
        async {
            let result = end.await;
            done.set(true);
            result
        },
        async {
            while !done.get() {
                pair.client
                    .drive(cx, Duration::from_millis(1), || true)
                    .await
                    .unwrap();
                pair.client
                    .receive(
                        cx,
                        || true,
                        |route, bytes| {
                            assert_eq!(
                                route,
                                Route::Stream(StreamRoute {
                                    outbound: false,
                                    ..pair.control
                                })
                            );
                            received.push(bytes.to_vec());
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
            }
        },
    ))
    .await;
    (outcome, received)
}
#[test]
fn closed_reports_preserve_unknown_and_known_effects_without_sending_queued_media() {
    use fr_wire::closure::{self, Cleanup, OutstandingEffects};
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for report in [
            closed_report(),
            closure::Closed {
                cleanup: Cleanup::Complete,
                effects: OutstandingEffects::Known {
                    pending: 2,
                    uncertain: u32::MAX,
                },
                ..closed_report()
            },
        ] {
            let mut pair = pair(&cx).await;
            pair.server
                .send(
                    &cx,
                    Route::Stream(pair.bulk),
                    &bulk(),
                    clock(&cx) + 1_000_000,
                    || true,
                )
                .unwrap();
            let original = pair.server.binding();
            let end =
                pair.server
                    .close_with_closed(&cx, &original, pair.control, binding(), report);
            assert!(pair.server.is_closed());
            assert_eq!(pair.server.usage().retained_send_records, 0);
            let (outcome, received) = collect_terminal(&cx, &mut pair, end).await;
            assert_eq!(outcome, Ok(()));
            assert_eq!(received.len(), 1);
            assert_eq!(received[0].len(), closure::CLOSED_BYTES);
            assert_eq!(
                closure::decode_closed(
                    &received[0],
                    binding(),
                    &ProtocolLimits::ABSOLUTE,
                    InputDirection::HostToViewer,
                    InputDelivery::Reliable
                ),
                Ok(report)
            );
            assert_eq!(pair.server.tick(&cx, || true), Err(Error::Closed));
        }
    });
}
#[test]
fn closed_report_abandonment_expiry_and_cancel_never_restart_the_original_owner() {
    for action in 0..3 {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut pair = pair(&cx).await;
            let original = pair.server.binding();
            let end = pair.server.close_with_closed(
                &cx,
                &original,
                pair.control,
                binding(),
                closed_report(),
            );
            assert!(pair.server.is_closed());
            match action {
                0 => drop(end),
                1 => {
                    asupersync::time::sleep(cx.now(), Duration::from_millis(260)).await;
                    assert_eq!(end.await, Err(Error::Expired));
                }
                _ => {
                    cx.cancel_fast(CancelKind::User);
                    assert_eq!(end.await, Err(Error::Cancelled));
                }
            }
            assert!(pair.server.is_closed());
        });
    }
}
#[test]
fn closed_report_wrong_proof_is_nonmutating_but_bad_matched_reports_close() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let foreign = pair.client.binding();
        assert_eq!(
            pair.server
                .close_with_closed(&cx, &foreign, pair.control, binding(), closed_report())
                .await,
            Err(Error::WrongRoute)
        );
        assert_eq!(pair.server.tick(&cx, || true), Ok(()));
        let original = pair.server.binding();
        assert_eq!(
            pair.server
                .close_with_closed(
                    &cx,
                    &original,
                    pair.control,
                    Binding {
                        session: RemoteSessionId::from_raw(0),
                        ..binding()
                    },
                    closed_report()
                )
                .await,
            Err(Error::Malformed)
        );
        assert!(pair.server.is_closed());
    });
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let original = pair.server.binding();
        assert_eq!(
            pair.server
                .close_with_closed(&cx, &original, pair.bulk, binding(), closed_report())
                .await,
            Err(Error::WrongRoute)
        );
        assert!(pair.server.is_closed());
    });
}
#[test]
fn closed_report_does_not_flush_partially_staged_payload_or_ignore_the_security_gate() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        pair.server
            .send(
                &cx,
                Route::Stream(pair.bulk),
                &bulk(),
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        pair.server
            .drive(&cx, Duration::ZERO, || true)
            .await
            .unwrap();
        let original = pair.server.binding();
        assert_eq!(
            pair.server
                .close_with_closed(&cx, &original, pair.control, binding(), closed_report())
                .await,
            Err(Error::Backpressure)
        );
        assert!(pair.server.is_closed());
    });
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let allowed = Arc::new(AtomicBool::new(true));
        let guard = allowed.clone();
        pair.server
            .retain_lifetime_check(&cx, Arc::new(move || guard.load(Ordering::Acquire)))
            .unwrap();
        let original = pair.server.binding();
        let end =
            pair.server
                .close_with_closed(&cx, &original, pair.control, binding(), closed_report());
        allowed.store(false, Ordering::Release);
        assert_eq!(end.await, Err(Error::Unauthorized));
    });
}
#[test]
fn armed_revocation_is_not_replaced_by_a_competing_closed_report() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut pair = pair(&cx).await;
        let original = pair.server.binding();
        let report_owner = RevocationReport::default();
        let fenced = Arc::new(AtomicBool::new(false));
        let check = fenced.clone();
        pair.server
            .arm_revocation_report(
                &cx,
                &original,
                pair.control,
                binding(),
                report().lease,
                report_owner.registration(),
                move || {
                    assert!(!check.swap(true, Ordering::AcqRel), "fence ran twice");
                    Reason::LocalRevoke
                },
            )
            .unwrap();
        let end =
            pair.server
                .close_with_closed(&cx, &original, pair.control, binding(), closed_report());
        assert!(pair.server.is_closed());
        assert!(fenced.load(Ordering::Acquire));
        assert_eq!(end.await, Err(Error::InvalidPolicy));
        let (outcome, received) = collect_terminal(&cx, &mut pair, async move {
            report_owner.finish().await.expect("armed report")
        })
        .await;
        assert_eq!(outcome, Ok(()));
        assert_eq!(received.len(), 1);
        assert_eq!(
            lease_revoked::decode(
                &received[0],
                binding(),
                report().lease,
                &ProtocolLimits::ABSOLUTE,
                InputDirection::HostToViewer,
                InputDelivery::Reliable
            ),
            Ok(report())
        );
    });
}
