//! The production viewer dispatcher over real UDP/TLS. Media bytes and decoder
//! completion are explicit fixtures; these tests do not claim hardware evidence.
use super::*;
use crate::session_startup::running::HostSession;
use crate::session_startup::viewer::streaming::quality::ViewerQuality;
use fr_wire::quality_decision::{self as wire, Decision, Mode, Reason};

struct Pair {
    host: HostSession,
    peer: Peer,
    receiver: ReceivePipeline,
    quality: Option<ViewerQuality>,
    binding: decoder::Binding,
    repairs: Repair,
    statistics: Statistics,
    nonce: u128,
}

async fn pair(c: &Cx, h: &Cx, selected: bool) -> Pair {
    let mut caps = capabilities();
    if selected {
        caps.push(negotiation::Capability {
            name: wire::CAPABILITY.into(),
            version: wire::VERSION,
            required: false,
        });
    }
    caps.sort_by(|a, b| a.name.cmp(&b.name));
    let (mut host, mut viewer) = pair_initialized(c, h, caps, |_| {}).await;
    let (_, media) = media_pair(&mut host, &mut viewer, c, h).await;
    let receiver = running(
        media
            .receiver_config(&viewer.transport, ReceivePolicy::default())
            .unwrap(),
        now(c).unwrap(),
    );
    let mut binding = media.binding();
    binding.parent = viewer.opened.binding;
    let mut quality = ViewerQuality::attach(
        &viewer.opened.selection,
        viewer.opened.binding,
        media.binding(),
        viewer.routes.inbound,
    )
    .unwrap();
    if let Some(quality) = &mut quality {
        quality.set_lifetime(receiver.receiver_lifetime());
    }
    Pair {
        host,
        peer: Peer::Observe {
            session: viewer,
            media,
        },
        receiver,
        quality,
        binding,
        repairs: Repair::default(),
        statistics: Statistics::default(),
        nonce: 100_000,
    }
}

fn decision(sequence: u64) -> Decision {
    Decision {
        sequence,
        capture_interval_us: 50_000,
        mode: Mode::Active,
        reason: Reason::SourceWork,
        applied_bitrate_bps: Some(2_000_000),
        pending_bitrate_bps: Some(1_500_000),
    }
}

async fn send(pair: &mut Pair, c: &Cx, h: &Cx, binding: decoder::Binding, value: Decision) {
    let mut bytes = [0; wire::RECORD_BYTES];
    let n = wire::encode(
        value,
        binding,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        input::InputDirection::HostToViewer,
        input::InputDelivery::Reliable,
    )
    .unwrap();
    // The first send can meet backpressure. Production retries it, so drive
    // both sides until the record is admitted, within its original deadline.
    let send_by = now(h).unwrap() + 1_000_000;
    loop {
        let (q, routes) = pair.host.io().unwrap();
        match q.send(
            h,
            Route::Stream(routes.outbound),
            &bytes[..n],
            send_by,
            || true,
        ) {
            Ok(()) => return,
            Err(quic::Error::Backpressure) => {
                assert!(now(h).unwrap() < send_by, "quality record never admitted");
                turn(pair, c).await.unwrap();
            }
            Err(error) => panic!("quality record send failed: {error:?}"),
        }
    }
}

async fn turn(pair: &mut Pair, c: &Cx) -> Result<(), Error> {
    let Pair {
        host,
        peer,
        receiver,
        quality,
        repairs,
        statistics,
        nonce,
        ..
    } = pair;
    let (host_result, client_result) = Box::pin(support::both(
        host.drive(
            Duration::from_millis(1),
            || {
                *nonce += 1;
                Ok(*nonce)
            },
            block,
        ),
        network(
            peer,
            None,
            None,
            receiver,
            None,
            repairs,
            None,
            statistics,
            None,
            quality.as_mut(),
            None,
            None,
            None,
            c,
            &mut |_| panic!("quality is not input"),
            &mut |_, bytes| {
                assert!(
                    !super::super::quality::is_quality(bytes),
                    "owned advisory escaped dispatch"
                );
                Ok(Disposition::Blocked)
            },
        ),
    ))
    .await;
    if client_result.is_ok() {
        host_result.unwrap();
    }
    client_result
}

#[test]
fn authenticated_quality_reports_do_not_change_media_or_input_evidence() {
    run(|c, h| async move {
        let mut pair = Box::pin(pair(&c, &h, true)).await;
        let usage = pair.receiver.budget_usage();
        let state = pair.receiver.state();
        let handle = pair
            .quality
            .as_ref()
            .unwrap()
            .handle(StreamingViewerControl::observing(c.clone()));
        assert_eq!(handle.current(), None);
        let binding = pair.binding;
        send(&mut pair, &c, &h, binding, decision(1)).await;
        let until = now(&c).unwrap() + 1_000_000;
        while pair.quality.as_ref().unwrap().received == 0 {
            assert!(
                now(&c).unwrap() < until,
                "quality record was never dispatched"
            );
            turn(&mut pair, &c).await.unwrap();
        }
        assert_eq!(pair.quality.as_ref().unwrap().current(), Some(decision(1)));
        assert_eq!(handle.current(), Some(decision(1)));
        assert_eq!(pair.receiver.budget_usage(), usage);
        assert_eq!(pair.receiver.state(), state);
        assert_eq!(pair.statistics.decoded, 0);
        assert_eq!(pair.statistics.compositor_submissions, 0);
        assert_eq!(pair.statistics.presented_age.reports(), 0);
        assert_eq!(pair.statistics.repair_requests, 0);
        assert!(pair.peer.controlled().is_none());
        // The original receiver fences its epoch immediately, before the
        // quality owner gets its next recovery turn. No stale report escapes.
        assert!(
            pair.receiver
                .receive(Channel::Video, &[], now(&c).unwrap())
                .is_err()
        );
        assert_eq!(
            pair.receiver.state(),
            fr_media::delivery::ReceiveState::NeedsRecovery
        );
        assert_eq!(handle.current(), None);
        assert_eq!(pair.quality.as_ref().unwrap().last(), Some(decision(1)));
        pair.host.close();
        pair.peer.close();
        pair.receiver.close();
    });
}

#[test]
fn current_quality_handle_cannot_outlive_cancellation_close_or_its_original_owner() {
    for ending in 0..3 {
        run(move |c, h| async move {
            let mut pair = Box::pin(pair(&c, &h, true)).await;
            let handle = pair
                .quality
                .as_ref()
                .unwrap()
                .handle(StreamingViewerControl::observing(c.clone()));
            let binding = pair.binding;
            send(&mut pair, &c, &h, binding, decision(1)).await;
            let until = now(&c).unwrap() + 1_000_000;
            while handle.current().is_none() {
                assert!(now(&c).unwrap() < until);
                turn(&mut pair, &c).await.unwrap();
            }
            match ending {
                0 => c.cancel_fast(CancelKind::User),
                1 => pair.quality.as_mut().unwrap().close(),
                _ => {
                    drop(pair.quality.take());
                }
            }
            assert_eq!(handle.current(), None);
            pair.host.close();
            pair.peer.close();
            pair.receiver.close();
        });
    }
}

#[test]
fn an_unselected_or_foreign_quality_report_is_a_named_terminal_protocol_refusal() {
    for selected in [false, true] {
        run(move |c, h| async move {
            let mut pair = Box::pin(pair(&c, &h, selected)).await;
            let mut binding = pair.binding;
            if selected {
                binding.display += 1;
            }
            send(&mut pair, &c, &h, binding, decision(1)).await;
            let until = now(&c).unwrap() + 1_000_000;
            let error = loop {
                assert!(
                    now(&c).unwrap() < until,
                    "unexpected advisory was not refused"
                );
                if let Err(error) = turn(&mut pair, &c).await {
                    break error;
                }
            };
            assert_eq!(
                error,
                Error::Quality(if selected {
                    QualityFault::Wire(WireError::InvalidBinding)
                } else {
                    QualityFault::NotSelected
                })
            );
            assert!(pair.quality.as_ref().is_none_or(|q| q.current().is_none()));
            pair.host.close();
            pair.peer.close();
            pair.receiver.close();
        });
    }
}
