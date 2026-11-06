//! The actual native host service loop under real QUIC queue backpressure.
//! Startup/IPC are real; opaque codec bytes and the requested loss are fixtures.
use super::*;
use fr_media::delivery::SendError;
use fr_wire::{Channel, Record, RepairRange, decode_fragment, encode_repair};

async fn pump(
    host: &mut HostSession,
    viewer: &mut ViewerSession,
    channels: &NegotiatedMedia,
    c: &Cx,
    n: &mut u128,
    fragments: &mut Vec<u32>,
) {
    let (a, b) = Box::pin(support::both(
        host.drive(Duration::from_millis(1), || nonce(n), block),
        viewer.drive(Duration::from_millis(1), block),
    ))
    .await;
    a.unwrap();
    b.unwrap();
    channels
        .receive_ready(
            c,
            viewer.io().unwrap().0,
            || true,
            |channel, bytes| {
                if channel == Channel::Video {
                    let record = Record::decode(
                        bytes,
                        &channels.limits(),
                        channels.bindings().for_channel(channel),
                        channel,
                    )
                    .unwrap();
                    fragments.push(decode_fragment(record, &channels.limits()).unwrap().index);
                }
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
}

#[test]
#[allow(clippy::too_many_lines)]
fn native_backpressure_retries_preserve_the_next_repair_turn() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair_initialized(&c, &h, capabilities(), |_| {}).await;
        let Media {
            mut stream,
            channels,
            ..
        } = Box::pin(media(
            &mut host,
            &mut viewer,
            &c,
            &h,
            "fragmented",
            SendPolicy::default(),
        ))
        .await;
        let mut n = 2000;
        let mut fragments = Vec::new();
        // Do not let bootstrap's reliable ACK hold the new announcement. The
        // new picture's original 250-ms deadline starts at the next capture.
        let bootstrap_until = now(&h).unwrap() + 1_000_000;
        while host.io().unwrap().0.usage().retained_send_records != 0 {
            assert!(
                now(&h).unwrap() < bootstrap_until,
                "bootstrap ACK did not drain"
            );
            pump(
                &mut host,
                &mut viewer,
                &channels,
                &c,
                &mut n,
                &mut fragments,
            )
            .await;
        }
        let control = stream.control.clone();
        let update = stream
            .source
            .capture_if_changed(&control, false)
            .await
            .unwrap();
        let frame = update.encoded().unwrap().frame().as_raw();
        stream.sender.enqueue_capture(update).unwrap();
        let mut originals = 0;
        let pending = loop {
            match stream
                .sender
                .transmit(&h, host.io().unwrap().0, Lane::Original)
                .unwrap()
            {
                Progress::Accepted(offer) if offer.channel() == Channel::Video => {
                    originals += 1;
                }
                Progress::Accepted(_) => {}
                Progress::Pending(offer) => break offer,
                Progress::Idle => panic!("fixture must exceed real native datagram credit"),
            }
        };
        assert!(originals > 0);
        assert_eq!(pending.channel(), Channel::Video);
        assert_eq!(pending.expiry_error(), SendError::OriginalExpired);
        let Route::Stream(repair) = stream.sender.stream_repair_route() else {
            unreachable!();
        };
        let mut request = vec![0; channels.limits().record_bytes()];
        let length = encode_repair(
            frame,
            &[RepairRange { start: 0, end: 1 }],
            channels.limits().max_fragments(),
            repair.binding,
            &channels.limits(),
            &mut request,
        )
        .unwrap();
        assert_eq!(
            stream
                .sender
                .repair(Route::Stream(repair), &request[..length]),
            Ok(crate::media_quic::RepairAdmission::Queued)
        );
        let (credit, _requests) = mpsc::channel(1);
        let (_completed, results) = mpsc::channel(1);
        let mut other = block;
        let mut services = VideoServices {
            control: &control,
            sender: &mut stream.sender,
            statistics: &mut stream.statistics,
            policy: Policy {
                records_per_turn: 1,
                ..stream.policy
            },
            capacity: stream.capacity,
            credit,
            results,
            in_flight: None,
            native_target_check: None,
            send_evidence: send_evidence::SendEvidence::default(),
            pacing: None,
            bitrate: None,
            rate_in_flight: None,
            last_work: None,
            last_capture: None,
            next_capture: u64::MAX,
            input_wake: super::super::super::input_wake::Wake::default(),
            feedback: None,
            presentation: None,
            cursor: None,
            media: None,
            other: &mut other,
        };
        // These are production service retries against genuinely full native
        // credit, not recording callbacks that simply claim backpressure.
        for _ in 0..2 {
            assert!(now(&h).unwrap() < pending.send_by_micros());
            services
                .maintain(host.io().unwrap().0, &mut || nonce(&mut n))
                .unwrap();
            assert_eq!(services.sender.pending(), Some(&pending));
            assert_eq!(services.statistics.admitted_records, 0);
        }
        // Drain the same prepared original without another scheduler decision.
        // Its acceptance, rather than the number of earlier polls, determines
        // which lane the next production service turn must select.
        loop {
            assert!(
                now(&h).unwrap() < pending.send_by_micros(),
                "original credit did not return"
            );
            pump(
                &mut host,
                &mut viewer,
                &channels,
                &c,
                &mut n,
                &mut fragments,
            )
            .await;
            match services
                .sender
                .transmit(&h, host.io().unwrap().0, Lane::Original)
                .unwrap()
            {
                Progress::Accepted(offer) => {
                    assert_eq!(offer, pending);
                    break;
                }
                Progress::Pending(offer) => assert_eq!(offer, pending),
                Progress::Idle => panic!("the unchanged original may not disappear"),
            }
        }
        loop {
            assert!(
                now(&h).unwrap() < pending.send_by_micros(),
                "repair credit did not return"
            );
            services
                .maintain(host.io().unwrap().0, &mut || nonce(&mut n))
                .unwrap();
            if let Some(offer) = services.sender.pending() {
                assert_eq!(offer.expiry_error(), SendError::RepairExpired);
                assert_eq!(offer.send_by_micros(), pending.send_by_micros());
            }
            if services.statistics.admitted_records != 0 {
                break;
            }
            pump(
                &mut host,
                &mut viewer,
                &channels,
                &c,
                &mut n,
                &mut fragments,
            )
            .await;
        }
        assert_eq!(services.statistics.admitted_records, 1);
        while fragments.len() < originals + 2 {
            assert!(
                now(&h).unwrap() < pending.send_by_micros(),
                "admitted fragments did not arrive"
            );
            pump(
                &mut host,
                &mut viewer,
                &channels,
                &c,
                &mut n,
                &mut fragments,
            )
            .await;
        }
        assert_eq!(
            fragments,
            (0..=u32::try_from(originals).unwrap())
                .chain(std::iter::once(0))
                .collect::<Vec<_>>(),
            "an eligible repair must precede the remaining busy original tail"
        );
        assert!(control.check().is_ok());
        assert!(!host.io().unwrap().0.is_closed());
        drop(services);
        host.close();
        stream
            .reap(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}
