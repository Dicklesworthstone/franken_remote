//! Original transport custody must survive blocked consumers and worker hops.
use super::*;

fn route(binding: u32, kind: u16) -> DatagramRoute {
    DatagramRoute {
        binding,
        kind,
        outbound: false,
    }
}
fn payload(value: u8) -> Bytes {
    Bytes::from(vec![value; 24])
}

#[test]
fn blocked_delivery_and_worker_handoffs_never_restart_receipt_custody() {
    let audio = route(1, AUDIO_PACKET);
    let mut inbox = PendingDatagrams::new();
    inbox.hold(audio, payload(7), 10, 100).unwrap();
    let mut original = None;
    for now in [10, 40, 100] {
        assert_eq!(
            inbox.receive_one(now, &mut |r, bytes, receipt| {
                assert_eq!(r, Route::Datagram(audio));
                assert_eq!(bytes, &[7; 24]);
                assert_eq!(receipt.received_at_micros(), 10);
                assert_eq!(receipt.expires_at_micros(), 110);
                if let Some(first) = original {
                    assert_eq!(receipt, first);
                }
                original = Some(receipt);
                Ok(Disposition::Blocked)
            }),
            Ok(0)
        );
    }
    // Transfer to a bounded worker inbox after most of the original allowance
    // was spent. Copying this content-free metadata cannot refresh it.
    let mut handed_off = None;
    assert_eq!(
        inbox.receive_one(105, &mut |_, _, receipt| {
            handed_off = Some(receipt);
            Ok(Disposition::Consumed)
        }),
        Ok(1)
    );
    let worker = handed_off.unwrap();
    assert_eq!(worker, original.unwrap());
    assert_eq!(worker.remaining_micros(105), Ok(5));
    assert_eq!(worker.remaining_micros(109), Ok(1));
    assert_eq!(worker.remaining_micros(110), Err(Error::Expired));
    assert_eq!(worker.remaining_micros(200), Err(Error::Expired));
    assert_eq!(inbox.bytes(), 0);
}

#[test]
fn promoted_audio_retains_its_own_first_receipt_and_smaller_negotiated_bound() {
    let audio = route(1, AUDIO_PACKET);
    let mut inbox = PendingDatagrams::new();
    inbox.hold(audio, payload(1), 0, 2_000_000).unwrap();
    inbox.hold(audio, payload(2), 90_000, 2_000_000).unwrap();
    inbox.hold(audio, payload(3), 95_000, 10_000).unwrap();
    // The first packet expires at the audio ceiling. The oldest overflow
    // packet becomes head, still tied to its own receipt and 100ms ceiling.
    inbox.expire(100_000);
    assert_eq!(inbox.dropped_audio, 1);
    assert_eq!(
        inbox.receive_one(101_000, &mut |_, bytes, receipt| {
            assert_eq!(bytes, &[2; 24]);
            assert_eq!(receipt.received_at_micros(), 90_000);
            assert_eq!(receipt.expires_at_micros(), 190_000);
            assert_eq!(receipt.remaining_micros(101_000), Ok(89_000));
            Ok(Disposition::Consumed)
        }),
        Ok(1)
    );
    let mut shorter = None;
    assert_eq!(
        inbox.receive_one(104_999, &mut |_, bytes, receipt| {
            assert_eq!(bytes, &[3; 24]);
            assert_eq!(receipt.received_at_micros(), 95_000);
            assert_eq!(receipt.expires_at_micros(), 105_000);
            shorter = Some(receipt);
            Ok(Disposition::Blocked)
        }),
        Ok(0)
    );
    assert_eq!(
        shorter.unwrap().remaining_micros(105_000),
        Err(Error::Expired)
    );
    assert_eq!(
        inbox.receive_one(105_000, &mut |_, _, _| panic!("expired audio")),
        Ok(0)
    );
    assert_eq!(inbox.dropped_audio, 2);
}

#[test]
fn parked_video_keeps_arrival_before_the_older_packet_expired() {
    let video = route(2, 0x0034);
    let mut inbox = PendingDatagrams::new();
    assert!(inbox.hold(video, payload(1), 10, 100).unwrap());
    assert!(!inbox.hold(video, payload(2), 100, 20).unwrap());
    assert!(inbox.waiting(109));
    assert!(!inbox.waiting(110));
    assert_eq!(
        inbox.receive_one(115, &mut |_, bytes, receipt| {
            assert_eq!(bytes, &[2; 24]);
            assert_eq!(receipt.received_at_micros(), 100);
            assert_eq!(receipt.expires_at_micros(), 120);
            assert_eq!(receipt.remaining_micros(119), Ok(1));
            assert_eq!(receipt.remaining_micros(120), Err(Error::Expired));
            Ok(Disposition::Consumed)
        }),
        Ok(1)
    );
}

#[test]
fn receipt_checks_refuse_a_regressed_clock_and_inclusive_expiry() {
    let mut inbox = PendingDatagrams::new();
    inbox
        .hold(route(1, AUDIO_PACKET), payload(1), 200, 30)
        .unwrap();
    assert_eq!(
        inbox.receive_one(200, &mut |_, _, receipt| {
            assert_eq!(receipt.remaining_micros(199), Err(Error::Clock));
            assert_eq!(receipt.remaining_micros(200), Ok(30));
            assert_eq!(receipt.remaining_micros(229), Ok(1));
            assert_eq!(receipt.remaining_micros(230), Err(Error::Expired));
            assert_eq!(receipt.remaining_micros(u64::MAX), Err(Error::Expired));
            Ok(Disposition::Consumed)
        }),
        Ok(1)
    );
}

#[test]
fn retired_audio_cannot_export_an_old_receipt_as_a_replacement() {
    let old = route(1, AUDIO_PACKET);
    let new = route(2, AUDIO_PACKET);
    let mut inbox = PendingDatagrams::new();
    inbox.hold(old, payload(1), 0, 100).unwrap();
    inbox.hold(old, payload(2), 10, 100).unwrap();
    inbox.retire(old.binding);
    assert_eq!(inbox.bytes(), 0);
    inbox.hold(new, payload(3), 20, 30).unwrap();
    assert_eq!(
        inbox.receive_one(21, &mut |r, bytes, receipt| {
            assert_eq!(r, Route::Datagram(new));
            assert_eq!(bytes, &[3; 24]);
            assert_eq!(receipt.received_at_micros(), 20);
            assert_eq!(receipt.expires_at_micros(), 50);
            Ok(Disposition::Blocked)
        }),
        Ok(0)
    );
    inbox.clear();
    assert_eq!(
        inbox.receive_one(22, &mut |_, _, _| panic!("closed custody")),
        Ok(0)
    );
}
