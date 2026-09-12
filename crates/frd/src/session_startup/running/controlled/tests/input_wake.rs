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
