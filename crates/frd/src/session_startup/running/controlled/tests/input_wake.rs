//! Actual native-owner result collection and UDP/TLS media service. OS effects,
//! consent, clock correlation, visibility and codec bytes are explicit fixtures.
use super::*;
use fr_media::pacing::Mode;

#[allow(clippy::too_many_lines)]
fn exercise(adaptive: bool) {
    crate::session_startup::running::streaming::tests::run(|c, h| async move {
        let Fixture {
            mut host,
            mut viewer,
            driver,
            seat,
            effects,
            ..
        } = Box::pin(fixture(&c, &h, None)).await;
        let (mut stream, channels, mut receiver) = Box::pin(
            crate::session_startup::running::streaming::tests::idle_source_for_controlled(
                &mut host.session,
                &mut viewer.session,
                &c,
                &h,
            ),
        )
        .await;
        // Use the same slow baseline in the fixed-policy negative case. Adaptive
        // idle must advance actual capture before this normal opportunity.
        if !adaptive {
            stream.policy.capture_interval = Duration::from_millis(200);
        }
        let stop = host.control();
        let mut host = host.into_streaming(stream).unwrap();
        if adaptive {
            host.enable_adaptive_capture(Duration::from_millis(200))
                .unwrap();
        }
        let (mut n, mut t) = (2000, 5000);
        let ((), shutdown) = Box::pin(support::both(
            async {
                let start = now(&c).unwrap();
                let mut submitted_after = None;
                let mut wake_delay = None;
                let mut previous = receiver.latest_progress().unwrap().observed_micros;
                let (result, ()) = Box::pin(support::both(
                    host.serve(|| nonce(&mut n), || ticket(&mut t), block),
                    async {
                        while now(&c).unwrap() < start + 3_200_000 {
                            viewer.drive(&c).await;
                            channels
                                .receive_ready(
                                    &c,
                                    viewer.session.io().unwrap().0,
                                    || true,
                                    |channel, bytes| {
                                        receiver.receive(channel, bytes, now(&c).unwrap()).unwrap();
                                        Ok(Disposition::Consumed)
                                    },
                                )
                                .unwrap();
                            assert!(
                                receiver.take_decodable(now(&c).unwrap()).unwrap().is_none(),
                                "unchanged source must not manufacture an encoded picture"
                            );
                            let observed = receiver.latest_progress().unwrap().observed_micros;
                            if observed > previous {
                                if let Some(sent_after) = submitted_after {
                                    if wake_delay.is_none() {
                                        wake_delay = Some(observed - sent_after);
                                        assert_eq!(effects.lock().unwrap().events, vec![true]);
                                    }
                                } else if now(&c).unwrap() >= start + 1_500_000 {
                                    // Just AFTER a genuine idle source check: the next
                                    // normal capture is still an entire interval away.
                                    submitted_after = Some(observed);
                                    viewer.queue_key(&c, true);
                                }
                                previous = observed;
                            }
                        }
                        assert_eq!(viewer.receipts, 1);
                        assert!(viewer.tickets >= 3);
                        stop.stop(StopReason::LocalRevoke);
                    },
                ))
                .await;
                assert!(result.is_err());
                let delay = wake_delay.expect("native submission never reached a new source check");
                if adaptive {
                    assert!(
                        delay < 150_000,
                        "idle input waited for ordinary capture: {delay} us"
                    );
                    assert_eq!(
                        host.statistics().input_wake_captures,
                        1,
                        "one retained/replayed result must not trigger recurring captures"
                    );
                    assert_eq!(host.pacing().unwrap().report().unwrap().mode, Mode::Idle);
                } else {
                    // Source-service timestamps include variable delay AFTER
                    // raw admission, so their difference is not the exact raw
                    // scheduling interval. The admission counter is authoritative.
                    assert_eq!(host.statistics().input_wake_captures, 0);
                    assert!(host.pacing().is_none());
                    assert!(
                        host.statistics().unchanged_observations <= 17,
                        "input increased fixed-policy capture admissions"
                    );
                }
                assert_eq!(host.statistics().encoded_updates, 0);
                host.reap_media(
                    &c,
                    crate::worker::Deadline::after(&c, Duration::from_secs(1)).unwrap(),
                )
                .await
                .unwrap();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
        assert_eq!(effects.lock().unwrap().events, vec![true, false]);
    });
}

#[test]
fn collected_native_input_advances_one_actual_idle_capture_without_inventing_pixels() {
    exercise(true);
}
#[test]
fn fixed_pacing_is_not_overridden_by_native_input_and_cleanup_remains_live() {
    exercise(false);
}

#[derive(Default)]
struct WakeEvents(Vec<u64>);
impl Services for WakeEvents {
    fn input_submitted(&mut self, at_us: u64) {
        self.0.push(at_us);
    }
    fn receive(&mut self, _: Route, _: &[u8]) -> Result<Disposition, ()> {
        Ok(Disposition::Blocked)
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn unfinished_native_input_cannot_wake_but_collected_effect_bypasses_blocked_receipt_delivery() {
    run(|c, h| async move {
        use fr_wire::{
            input::{InputDelivery, InputDirection},
            input_result::{InputResult, ResultBinding, SequenceSpace, Stage, encode_input_result},
        };
        let (entered_tx, entered) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let Fixture {
            mut host,
            mut viewer,
            driver,
            seat,
            effects,
            ..
        } = Box::pin(fixture(&c, &h, Some((entered_tx, gate)))).await;
        let (mut n, mut t) = (2000, 5000);
        let mut events = WakeEvents::default();
        let ((), shutdown) = Box::pin(support::both(
            async {
                viewer.visible(&c);
                viewer.queue_key(&c, true);
                let until = now(&h).unwrap() + 500_000;
                while entered.try_recv().is_err() {
                    assert!(now(&h).unwrap() < until, "native input did not start");
                    turn(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                }
                // Fill the ORIGINAL reverse input stream with valid bounded
                // fixture records. No simulated receipt reaches native collection
                // or the viewer's result ledger; this is send-pressure only.
                let route = host.input.routes().results();
                let mut bytes = [0; 256];
                let count = encode_input_result(
                    InputResult {
                        binding: ResultBinding {
                            channel: route.binding,
                            session: credentials().session,
                            lease: credentials().lease,
                        },
                        sequence: 99,
                        space: SequenceSpace::Action,
                        stage: Stage::SubmittedToOs,
                        outcome: InputOutcome::SubmittedToOs,
                        submitted_operations: 1,
                        unknown_next_operation: false,
                        reason: None,
                    },
                    &mut bytes,
                    &ProtocolLimits::ABSOLUTE,
                    InputDirection::HostToViewer,
                    InputDelivery::Reliable,
                )
                .unwrap();
                let mut blocked = false;
                for _ in 0..256 {
                    match host.session.opened.transport.send(
                        &h,
                        Route::Stream(route),
                        &bytes[..count],
                        until,
                        || true,
                    ) {
                        Ok(()) => {}
                        Err(quic::Error::Backpressure) => {
                            blocked = true;
                            break;
                        }
                        Err(error) => panic!("fixture queue failure: {error:?}"),
                    }
                }
                assert!(blocked, "test never reached actual QUIC send backpressure");
                let mut tickets = || ticket(&mut t);
                let mut services = InputServices {
                    input: &mut host.input,
                    renewal: &mut host.renewal,
                    observation: host.session.opened.control.clone(),
                    control: host.session.opened.routes,
                    ticket_turn: &mut host.ticket_turn,
                    submitted: &mut host.submitted,
                    ticket: &mut tickets,
                    other: &mut events,
                };
                services
                    .maintain(&mut host.session.opened.transport, &mut || nonce(&mut n))
                    .unwrap();
                assert!(
                    services.other.0.is_empty(),
                    "queued/entered input woke capture"
                );
                assert_eq!(effects.lock().unwrap().events, [] as [bool; 0]);
                release.send(()).unwrap();
                while services.other.0.is_empty() {
                    assert!(now(&h).unwrap() < until, "native receipt was not collected");
                    services
                        .maintain(&mut host.session.opened.transport, &mut || nonce(&mut n))
                        .unwrap();
                    asupersync::time::sleep(h.now(), Duration::from_millis(1)).await;
                }
                assert!(services.input.pending_receipt().is_some());
                assert!(!services.input.can_accept_input());
                assert_eq!(effects.lock().unwrap().events, vec![true]);
                for _ in 0..32 {
                    services
                        .maintain(&mut host.session.opened.transport, &mut || nonce(&mut n))
                        .unwrap();
                }
                assert_eq!(
                    services.other.0.len(),
                    1,
                    "retained receipt replayed activity"
                );
                assert_eq!(
                    viewer.receipts, 0,
                    "test unexpectedly delivered reverse records"
                );
                host.close();
            },
            driver,
        ))
        .await;
        assert_eq!(events.0.len(), 1);
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
        assert_eq!(effects.lock().unwrap().events, vec![true, false]);
    });
}
