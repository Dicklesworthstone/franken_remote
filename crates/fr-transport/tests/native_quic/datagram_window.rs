//! Exercise admission against the actual TLS/UDP sender's congestion snapshot.
//! The loss-floor arithmetic is tested separately without pretending that a
//! localhost handshake is a measured impaired path.
use super::*;

fn fragment(index: u32) -> Vec<u8> {
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap();
    let data = [19; 1077];
    let mut bytes = vec![0; 1150];
    let length = fr_wire::encode_fragment(
        fr_wire::Fragment {
            descriptor: FrameDescriptor {
                frame: 1,
                reference: Some(0),
                total_bytes: 1077 * 128,
                stride: 1077,
                capture_micros: 0,
            },
            index,
            bytes: &data,
        },
        1,
        &limits,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(length);
    bytes
}

fn fill(cx: &Cx, pair: &mut support::Pair, count: u32, until: u64) {
    for index in 0..count {
        assert_eq!(
            pair.server.send(
                cx,
                Route::Datagram(pair.video),
                &fragment(index),
                until,
                || true,
            ),
            Ok(()),
            "video slot {index} was refused before the native window filled"
        );
    }
}

fn video_slots(room: u64) -> u32 {
    u32::try_from((room / 1200).min(128)).unwrap()
}

#[test]
fn video_uses_all_real_packet_room_without_an_idle_reliable_reservation() {
    run_test!(cx, {
        let (mut pair, room) = support::pair_with_send_room(&cx, Policy::default()).await;
        let slots = video_slots(room);
        assert!(slots >= 2, "handshake left no usable test flight: {room}");
        let until = clock(&cx) + 1_000_000;
        fill(&cx, &mut pair, slots, until);
        assert_eq!(
            pair.server.send(
                &cx,
                Route::Datagram(pair.video),
                &fragment(0),
                until,
                || true,
            ),
            Err(Error::Backpressure)
        );
        assert!(!pair.server.is_closed());
    });
}

#[test]
fn pending_critical_and_bulk_records_each_keep_one_real_packet_slot() {
    run_test!(cx, {
        for critical in [true, false] {
            let (mut pair, room) = support::pair_with_send_room(&cx, Policy::default()).await;
            let slots = video_slots(room);
            assert!((2..128).contains(&slots));
            let until = clock(&cx) + 1_000_000;
            let (route, bytes) = if critical {
                (pair.host_routes[0], record(3, 0x37, 100, 7))
            } else {
                (pair.host_routes[1], recovery_packet(2, 100))
            };
            pair.server
                .send(&cx, Route::Stream(route), &bytes, until, || true)
                .unwrap();
            fill(&cx, &mut pair, slots - 1, until);
            assert_eq!(
                pair.server.send(
                    &cx,
                    Route::Datagram(pair.video),
                    &fragment(0),
                    until,
                    || true,
                ),
                Err(Error::Backpressure)
            );
            let mut received = None;
            for _ in 0..20 {
                drive(&cx, &mut pair).await;
                pair.client
                    .receive(
                        &cx,
                        || true,
                        |incoming, payload| {
                            if matches!(incoming, Route::Stream(_)) {
                                received = Some(payload.to_vec());
                            }
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
                if received.is_some() {
                    break;
                }
            }
            assert_eq!(received.as_deref(), Some(bytes.as_slice()));
        }
    });
}

#[test]
fn last_video_slot_still_checks_original_expiry_and_prepared_record_validity() {
    run_test!(cx, {
        let (mut pair, room) = support::pair_with_send_room(&cx, Policy::default()).await;
        let slots = video_slots(room);
        assert!(slots >= 2);
        let until = clock(&cx) + 1_000_000;
        fill(&cx, &mut pair, slots - 1, until);
        let route = Route::Datagram(pair.video);
        let packet = fragment(slots - 1);
        assert_eq!(
            pair.server.send(&cx, route, &packet, clock(&cx), || true),
            Err(Error::Expired)
        );
        assert!(!pair.server.is_closed());
        assert_eq!(
            pair.server.send_prepared(
                &cx,
                route,
                &packet,
                until,
                || true,
                || Err("obsolete picture"),
            ),
            Ok(SendAdmission::Refused("obsolete picture"))
        );
        // Neither refusal admitted bytes or consumed the last native slot.
        pair.server
            .send(&cx, route, &packet, until, || true)
            .unwrap();
        assert_eq!(
            pair.server.send_prepared(
                &cx,
                route,
                &packet,
                until,
                || true,
                || -> Result<(), ()> { panic!("backpressure must precede preparation") },
            ),
            Err(Error::Backpressure)
        );
    });
}
