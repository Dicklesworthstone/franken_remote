//! Real closing exchange on UDP/TLS. Report stages remain explicit fixtures.
use super::*;
use fr_wire::closure::{self, Cleanup, CloseRequest, Closed, ClosedReason, OutstandingEffects};

fn routes(pair: &Pair) -> ControlRoutes {
    ControlRoutes {
        outbound: StreamRoute {
            outbound: true,
            ..pair.incoming
        },
        inbound: StreamRoute {
            outbound: false,
            ..pair.control
        },
    }
}
fn request() -> CloseRequest {
    CloseRequest {
        reason: closure::Reason::Requested,
    }
}
fn final_report() -> Closed {
    Closed {
        reason: ClosedReason::ClientRequested,
        cleanup: Cleanup::Unconfirmed,
        effects: OutstandingEffects::Unknown,
    }
}
fn request_bytes(reason: closure::Reason) -> Vec<u8> {
    let mut bytes = vec![0; closure::REQUEST_BYTES];
    closure::encode_request(
        CloseRequest { reason },
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes
}
async fn request_then_report(cx: &Cx, pair: &mut Pair, report: Closed) -> Result<(), Error> {
    let mut requests = Vec::new();
    while requests.is_empty() {
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
                    requests.push(
                        closure::decode_request(
                            bytes,
                            binding(),
                            &ProtocolLimits::ABSOLUTE,
                            InputDirection::ViewerToHost,
                            InputDelivery::Reliable,
                        )
                        .unwrap(),
                    );
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
    assert_eq!(requests, [request()]);
    let original = pair.server.binding();
    pair.server
        .close_with_closed(cx, &original, pair.control, binding(), report)
        .await
}

#[test]
fn closing_exchange_discards_unstaged_writes_and_preserves_exact_cleanup_uncertainty() {
    for report in [
        final_report(),
        Closed {
            cleanup: Cleanup::Complete,
            effects: OutstandingEffects::Known {
                pending: 2,
                uncertain: u32::MAX,
            },
            ..final_report()
        },
    ] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx).await;
            let routes = routes(&p);
            p.client
                .send(
                    &cx,
                    Route::Stream(routes.outbound),
                    &request_bytes(closure::Reason::InspectionComplete),
                    clock(&cx) + 1_000_000,
                    || true,
                )
                .unwrap();
            let original = p.client.binding();
            let exchange =
                p.client
                    .close_with_request(&cx, &original, routes, binding(), request(), u64::MAX);
            assert!(p.client.is_closed(), "detach before first poll");
            assert_eq!(p.client.usage().retained_send_records, 0);
            let (outcome, sent) =
                Box::pin(both(exchange, request_then_report(&cx, &mut p, report))).await;
            assert_eq!(outcome.report, Some(report));
            assert_eq!(outcome.transport, Ok(()));
            assert_eq!(sent, Ok(()), "client must ACK the received report");
            assert_eq!(p.client.tick(&cx, || true), Err(Error::Closed));
        });
    }
}
#[test]
fn request_ack_without_a_report_remains_unknown_and_cannot_renew_the_deadline() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let routes = routes(&p);
        let original = p.client.binding();
        let until = clock(&cx) + 60_000;
        let exchange =
            p.client
                .close_with_request(&cx, &original, routes, binding(), request(), until);
        let done = Cell::new(false);
        let mut count = 0;
        let (outcome, ()) = Box::pin(both(
            async {
                let result = exchange.await;
                done.set(true);
                result
            },
            async {
                while !done.get() {
                    p.server
                        .drive(&cx, Duration::from_millis(1), || true)
                        .await
                        .unwrap();
                    p.server
                        .receive(
                            &cx,
                            || true,
                            |_, bytes| {
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
                                count += 1;
                                Ok(Disposition::Consumed)
                            },
                        )
                        .unwrap();
                }
            },
        ))
        .await;
        assert_eq!(count, 1);
        assert!(outcome.request_acknowledged);
        assert_eq!(outcome.report, None);
        assert_eq!(outcome.transport, Err(Error::Expired));
        assert!(clock(&cx) >= until);
    });
}
#[test]
fn cancellation_abandonment_and_delayed_poll_do_not_restore_ordinary_io() {
    for action in 0..3 {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx).await;
            let routes = routes(&p);
            let original = p.client.binding();
            let operation =
                p.client
                    .close_with_request(&cx, &original, routes, binding(), request(), u64::MAX);
            assert!(p.client.is_closed());
            match action {
                0 => drop(operation),
                1 => {
                    asupersync::time::sleep(cx.now(), Duration::from_millis(260)).await;
                    assert_eq!(operation.await.transport, Err(Error::Expired));
                }
                _ => {
                    cx.cancel_fast(CancelKind::User);
                    assert_eq!(operation.await.transport, Err(Error::Cancelled));
                }
            }
            assert!(p.client.is_closed());
        });
    }
}
#[test]
fn foreign_proofs_refuse_without_closing_but_matched_wrong_roles_close() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let routes = routes(&p);
        let original = p.client.binding();
        let foreign = p.server.binding();
        assert_eq!(
            p.client
                .close_with_request(&cx, &foreign, routes, binding(), request(), u64::MAX)
                .await
                .transport,
            Err(Error::WrongRoute)
        );
        assert_eq!(p.client.tick(&cx, || true), Ok(()));
        let flipped = ControlRoutes {
            outbound: routes.inbound,
            inbound: routes.outbound,
        };
        assert_eq!(
            p.client
                .close_with_request(&cx, &original, flipped, binding(), request(), u64::MAX)
                .await
                .transport,
            Err(Error::WrongRoute)
        );
        assert!(p.client.is_closed());
        let original = p.server.binding();
        assert_eq!(
            p.server
                .close_with_request(&cx, &original, routes, binding(), request(), u64::MAX)
                .await
                .transport,
            Err(Error::WrongRoute)
        );
        assert!(p.server.is_closed());
    });
}
#[test]
fn native_outbound_data_is_never_flushed_as_part_of_the_close_request() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let routes = routes(&p);
        let original = p.client.binding();
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
        let result = p
            .client
            .close_with_request(&cx, &original, routes, binding(), request(), u64::MAX)
            .await;
        assert_eq!(result.transport, Err(Error::Backpressure));
        assert_eq!(result.report, None);
    });
}
#[test]
fn destination_security_loss_prevents_the_detached_exchange() {
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
        let exchange =
            p.client
                .close_with_request(&cx, &original, routes, binding(), request(), u64::MAX);
        allowed.store(false, Ordering::Release);
        assert_eq!(exchange.await.transport, Err(Error::Unauthorized));
    });
}
#[test]
fn malformed_or_foreign_session_reports_do_not_become_successful_closure() {
    for foreign in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx).await;
            let mut bytes = vec![0; closure::CLOSED_BYTES];
            let bound = if foreign {
                Binding {
                    session: RemoteSessionId::from_raw(999),
                    ..binding()
                }
            } else {
                binding()
            };
            closure::encode_closed(
                final_report(),
                bound,
                &ProtocolLimits::ABSOLUTE,
                &mut bytes,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
            .unwrap();
            if !foreign {
                bytes[42] = 0xff;
            } // invalid cleanup stage, valid frame header
            p.server
                .send(
                    &cx,
                    Route::Stream(p.control),
                    &bytes,
                    clock(&cx) + 1_000_000,
                    || true,
                )
                .unwrap();
            let routes = routes(&p);
            let original = p.client.binding();
            let exchange =
                p.client
                    .close_with_request(&cx, &original, routes, binding(), request(), u64::MAX);
            let done = Cell::new(false);
            let (outcome, ()) = Box::pin(both(
                async {
                    let r = exchange.await;
                    done.set(true);
                    r
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
            assert_eq!(outcome.report, None);
            assert_eq!(outcome.transport, Err(Error::Malformed));
        });
    }
}
#[test]
fn a_partial_original_control_record_is_preserved_not_reframed_after_detachment() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let mut bytes = vec![0; closure::CLOSED_BYTES];
        closure::encode_closed(
            final_report(),
            binding(),
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        // A valid optional extension makes the report span multiple native
        // 900-byte staging prefixes. Nothing is resynchronized at the next FRD0.
        let ext_len = 1008_u32;
        bytes[12..16].copy_from_slice(&(28 + ext_len).to_be_bytes());
        bytes[20..24].copy_from_slice(&ext_len.to_be_bytes());
        bytes.extend_from_slice(&1_u16.to_be_bytes());
        bytes.extend_from_slice(&0_u16.to_be_bytes());
        bytes.extend_from_slice(&1000_u32.to_be_bytes());
        bytes.resize(closure::CLOSED_BYTES + 1008, 3);
        p.server
            .send(
                &cx,
                Route::Stream(p.control),
                &bytes,
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        p.server.drive(&cx, Duration::ZERO, || true).await.unwrap();
        p.client
            .drive(&cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        assert_eq!(
            p.client
                .receive(&cx, || true, |_, _| panic!("only a prefix arrived")),
            Ok(0)
        );
        assert!(p.client.usage().framed_capacity > 0);
        let routes = routes(&p);
        let original = p.client.binding();
        let exchange =
            p.client
                .close_with_request(&cx, &original, routes, binding(), request(), u64::MAX);
        let done = Cell::new(false);
        let (outcome, ()) = Box::pin(both(
            async {
                let r = exchange.await;
                done.set(true);
                r
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
        assert_eq!(outcome.report, Some(final_report()));
        assert_eq!(outcome.transport, Ok(()));
    });
}

fn clock_reply(sequence: u64) -> Vec<u8> {
    let mut bytes = vec![0; fr_wire::clock::REPLY_BYTES];
    fr_wire::clock::encode(
        fr_wire::clock::Message::Reply {
            sequence,
            host_sample_us: 100,
        },
        fr_wire::negotiation::ControlBinding {
            id: binding().channel,
            host_boot: fr_core::ids::HostBootId::from_raw(1),
            os_session: fr_core::ids::OsSessionId::from_raw(2),
            remote_session: binding().session,
        },
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes
}

#[test]
fn a_control_flood_cannot_keep_the_closing_exchange_alive_or_trigger_application_responses() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let route = routes(&p);
        let original = p.client.binding();
        let exchange =
            p.client
                .close_with_request(&cx, &original, route, binding(), request(), u64::MAX);
        let done = Cell::new(false);
        let mut offered = 0;
        let mut requests = 0;
        let (result, ()) = Box::pin(both(
            async {
                let result = exchange.await;
                done.set(true);
                result
            },
            async {
                while !done.get() {
                    if offered < 40 {
                        match p.server.send(
                            &cx,
                            Route::Stream(p.control),
                            &clock_reply(offered + 1),
                            clock(&cx) + 1_000_000,
                            || true,
                        ) {
                            Ok(()) => offered += 1,
                            Err(Error::Backpressure) => {}
                            other => panic!("unexpected offer: {other:?}"),
                        }
                    }
                    p.server
                        .drive(&cx, Duration::from_millis(1), || true)
                        .await
                        .unwrap();
                    p.server
                        .receive(
                            &cx,
                            || true,
                            |_, bytes| {
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
                                requests += 1;
                                Ok(Disposition::Consumed)
                            },
                        )
                        .unwrap();
                }
            },
        ))
        .await;
        assert_eq!(result.transport, Err(Error::TooLarge));
        assert_eq!(result.report, None);
        assert_eq!(requests, 1);
        assert!(offered >= 32);
    });
}

#[test]
fn nothing_after_the_first_closed_record_can_replace_its_terminal_result() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let mut bytes = [0; closure::CLOSED_BYTES];
        closure::encode_closed(
            final_report(),
            binding(),
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        p.server
            .send(
                &cx,
                Route::Stream(p.control),
                &bytes,
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        bytes[42] = 0xff; // Invalid cleanup stage must never be parsed after Closed.
        p.server
            .send(
                &cx,
                Route::Stream(p.control),
                &bytes,
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        let route = routes(&p);
        let original = p.client.binding();
        let exchange =
            p.client
                .close_with_request(&cx, &original, route, binding(), request(), u64::MAX);
        let done = Cell::new(false);
        let (result, ()) = Box::pin(both(
            async {
                let result = exchange.await;
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
        assert_eq!(result.report, Some(final_report()));
        assert_eq!(result.transport, Ok(()));
    });
}
