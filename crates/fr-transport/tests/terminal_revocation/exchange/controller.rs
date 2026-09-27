//! Controller terminal reports use the SAME parser, credit and socket as closing.
//! Grant, release stages and effects are explicit fixtures, not native key proof.
use super::*;

async fn receive_request(cx: &Cx, pair: &mut Pair) {
    let until = clock(cx) + 250_000;
    let mut received = false;
    while !received {
        assert!(clock(cx) < until);
        pair.server
            .drive(cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        pair.server
            .receive(
                cx,
                || true,
                |route, bytes| {
                    assert_eq!(route, Route::Stream(pair.incoming));
                    assert_eq!(
                        closure::decode_request(
                            bytes,
                            binding(),
                            &ProtocolLimits::ABSOLUTE,
                            InputDirection::ViewerToHost,
                            InputDelivery::Reliable
                        )
                        .unwrap(),
                        request()
                    );
                    received = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
}

#[test]
fn exact_lease_report_ends_controller_exchange_without_fabricating_session_cleanup() {
    for cleanup in [
        CleanupStage::Fenced,
        CleanupStage::Released,
        CleanupStage::Failed,
    ] {
        for effects in [
            EffectStage::Unknown,
            EffectStage::ReceiptsPending,
            EffectStage::ReceiptsComplete,
        ] {
            runtime().block_on(async {
                let cx = Cx::current().unwrap();
                let mut p = pair(&cx).await;
                let expected = Revoked {
                    cleanup,
                    effects,
                    reason: Reason::ClientRequested,
                    ..report()
                };
                let routes = routes(&p);
                let original = p.client.binding();
                let close = p.client.close_control_with_request(
                    &cx,
                    &original,
                    routes,
                    binding(),
                    expected.lease,
                    request(),
                    u64::MAX,
                );
                assert!(
                    p.client.is_closed(),
                    "no input can be sent after construction"
                );
                let (outcome, sent) = Box::pin(both(close, async {
                    receive_request(&cx, &mut p).await;
                    let original = p.server.binding();
                    p.server
                        .close_with_revocation(&cx, &original, p.control, binding(), expected)
                        .await
                }))
                .await;
                assert_eq!(outcome.revocation, Some(expected));
                assert_eq!(outcome.exchange.report, None);
                assert_eq!(outcome.exchange.transport, Ok(()));
                assert_eq!(sent, Ok(()));
            });
        }
    }
}

#[test]
fn controller_can_receive_closed_without_inventing_a_lease_release() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let original = p.client.binding();
        let routes = routes(&p);
        let close = p.client.close_control_with_request(
            &cx,
            &original,
            routes,
            binding(),
            report().lease,
            request(),
            u64::MAX,
        );
        let (outcome, sent) = Box::pin(both(
            close,
            request_then_report(&cx, &mut p, final_report()),
        ))
        .await;
        assert_eq!(outcome.exchange.report, Some(final_report()));
        assert_eq!(outcome.revocation, None);
        assert_eq!(outcome.exchange.transport, Ok(()));
        assert_eq!(sent, Ok(()));
    });
}

#[test]
fn foreign_lease_session_or_invalid_stage_never_becomes_terminal_evidence() {
    for invalid in 0..3 {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx).await;
            let mut bytes = [0; REVOKED_BYTES];
            let mut revoked = report();
            let mut bound = binding();
            match invalid {
                0 => revoked.lease = InputLeaseId::from_raw(100),
                1 => bound.session = RemoteSessionId::from_raw(100),
                _ => {}
            }
            lease_revoked::encode(
                revoked,
                bound,
                &ProtocolLimits::ABSOLUTE,
                &mut bytes,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
            .unwrap();
            if invalid == 2 {
                bytes[REVOKED_BYTES - 2] = 0xff;
            }
            p.server
                .send(
                    &cx,
                    Route::Stream(p.control),
                    &bytes,
                    clock(&cx) + 1_000_000,
                    || true,
                )
                .unwrap();
            let original = p.client.binding();
            let routes = routes(&p);
            let close = p.client.close_control_with_request(
                &cx,
                &original,
                routes,
                binding(),
                report().lease,
                request(),
                u64::MAX,
            );
            let done = Cell::new(false);
            let (outcome, ()) = Box::pin(both(
                async {
                    let result = close.await;
                    done.set(true);
                    result
                },
                async {
                    while !done.get() {
                        p.server
                            .drive(&cx, Duration::from_millis(1), || true)
                            .await
                            .unwrap();
                    }
                },
            ))
            .await;
            assert_eq!(outcome.revocation, None);
            assert_eq!(outcome.exchange.report, None);
            assert_eq!(outcome.exchange.transport, Err(Error::Malformed));
        });
    }
}

#[test]
fn only_the_first_terminal_report_is_retained_even_if_another_is_already_queued() {
    for lease_first in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx).await;
            let mut revoked = [0; REVOKED_BYTES];
            lease_revoked::encode(
                report(),
                binding(),
                &ProtocolLimits::ABSOLUTE,
                &mut revoked,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
            .unwrap();
            let mut closed = [0; closure::CLOSED_BYTES];
            closure::encode_closed(
                final_report(),
                binding(),
                &ProtocolLimits::ABSOLUTE,
                &mut closed,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
            .unwrap();
            let records = if lease_first {
                [revoked.as_slice(), closed.as_slice()]
            } else {
                [closed.as_slice(), revoked.as_slice()]
            };
            for bytes in records {
                p.server
                    .send(
                        &cx,
                        Route::Stream(p.control),
                        bytes,
                        clock(&cx) + 1_000_000,
                        || true,
                    )
                    .unwrap();
            }
            let original = p.client.binding();
            let routes = routes(&p);
            let close = p.client.close_control_with_request(
                &cx,
                &original,
                routes,
                binding(),
                report().lease,
                request(),
                u64::MAX,
            );
            let done = Cell::new(false);
            let (outcome, ()) = Box::pin(both(
                async {
                    let result = close.await;
                    done.set(true);
                    result
                },
                async {
                    while !done.get() {
                        p.server
                            .drive(&cx, Duration::from_millis(1), || true)
                            .await
                            .unwrap();
                    }
                },
            ))
            .await;
            assert_eq!(outcome.revocation, lease_first.then_some(report()));
            assert_eq!(
                outcome.exchange.report,
                (!lease_first).then_some(final_report())
            );
            assert_eq!(outcome.exchange.transport, Ok(()));
        });
    }
}

#[test]
fn controller_close_preserves_native_backlog_refusal_and_foreign_proof_nonmutation() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let routes = routes(&p);
        let original = p.client.binding();
        let foreign = p.server.binding();
        let outcome = p
            .client
            .close_control_with_request(
                &cx,
                &foreign,
                routes,
                binding(),
                report().lease,
                request(),
                u64::MAX,
            )
            .await;
        assert_eq!(outcome.exchange.transport, Err(Error::WrongRoute));
        assert!(!p.client.is_closed());
        p.client
            .send(
                &cx,
                Route::Stream(routes.outbound),
                &request_bytes(closure::Reason::ClientFailure),
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        p.client.drive(&cx, Duration::ZERO, || true).await.unwrap();
        assert!(p.client.usage().retained_send_records > 0);
        let outcome = p
            .client
            .close_control_with_request(
                &cx,
                &original,
                routes,
                binding(),
                report().lease,
                request(),
                u64::MAX,
            )
            .await;
        assert_eq!(outcome.exchange.transport, Err(Error::Backpressure));
        assert_eq!(outcome.revocation, None);
        assert!(p.client.is_closed());
    });
}

#[test]
fn zero_lease_cancel_security_loss_and_delayed_poll_never_restart_controller_close() {
    for refusal in 0..4 {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx).await;
            let allowed = Arc::new(AtomicBool::new(true));
            let gate = allowed.clone();
            p.client
                .retain_lifetime_check(&cx, Arc::new(move || gate.load(Ordering::Acquire)))
                .unwrap();
            let routes = routes(&p);
            let original = p.client.binding();
            let lease = if refusal == 0 {
                InputLeaseId::from_raw(0)
            } else {
                report().lease
            };
            let close = p.client.close_control_with_request(
                &cx,
                &original,
                routes,
                binding(),
                lease,
                request(),
                clock(&cx) + 10_000,
            );
            assert!(p.client.is_closed());
            let expected = match refusal {
                0 => Error::WrongRoute,
                1 => {
                    cx.cancel_fast(CancelKind::User);
                    Error::Cancelled
                }
                2 => {
                    allowed.store(false, Ordering::Release);
                    Error::Unauthorized
                }
                _ => {
                    asupersync::time::sleep(cx.now(), Duration::from_millis(15)).await;
                    Error::Expired
                }
            };
            let outcome = close.await;
            assert_eq!(outcome.exchange.transport, Err(expected));
            assert_eq!(outcome.exchange.report, None);
            assert_eq!(outcome.revocation, None);
        });
    }
}
