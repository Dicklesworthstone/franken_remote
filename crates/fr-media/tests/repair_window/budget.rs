//! A consumed offered-load allowance is pacing, not a new reference failure.
use super::*;

fn policy(window: u64) -> SendPolicy {
    SendPolicy {
        repair_bytes_per_window: 1_150,
        repair_window_micros: window,
        max_repair_rounds: 1,
        ..SendPolicy::default()
    }
}

#[test]
fn a_partly_emitted_request_survives_pacing_without_another_receiver_attempt() {
    let mut s = sender(policy(50_000));
    populate(&mut s, 1, 3);
    s.queue_repair(&request(1, 0, 3, 3), 20_000).unwrap();
    let charged = s.cached_bytes();
    assert_eq!(repair(&mut s, 20_000), Some((1, 0)));
    assert_eq!(s.next_deadline(), Some(50_000));
    let mut output = [0xa5; 1_150];
    for now in [20_000, 20_001, 30_000, 49_999] {
        assert_eq!(s.next_repair_packet(now, &mut output), Ok(None));
        assert_eq!(output, [0xa5; 1_150]);
        assert_eq!(s.cached_bytes(), charged);
        assert_eq!(s.next_deadline(), Some(50_000));
    }
    for (now, index) in [(50_000, 1), (100_000, 2)] {
        let offer = s.next_repair_packet(now, &mut output).unwrap().unwrap();
        assert_eq!(fragment(&output[..offer.byte_len()]), (1, index));
        assert_eq!(offer.send_by_micros(), 251_000);
        s.authorize_write(&offer, now).unwrap();
    }
    // Both window refills resumed the original request, not another round.
    assert_eq!(
        s.queue_repair(&request(1, 0, 1, 3), 100_001),
        Err(SendError::RepairRateLimited)
    );
    assert_eq!(s.next_deadline(), Some(251_000));
    assert!(!s.needs_recovery());
}

#[test]
fn every_picture_shares_the_same_byte_allowance_without_a_new_burst_per_job() {
    let mut s = sender(policy(100_000));
    populate(&mut s, 2, 1);
    for frame in [2, 1] {
        s.queue_repair(&request(frame, 0, 1, 1), 20_000).unwrap();
    }
    assert_eq!(repair(&mut s, 20_000), Some((1, 0)));
    assert_eq!(repair(&mut s, 20_000), None);
    assert_eq!(s.next_deadline(), Some(100_000));
    assert_eq!(repair(&mut s, 99_999), None);
    assert_eq!(repair(&mut s, 100_000), Some((2, 0)));
    assert_eq!(repair(&mut s, 100_000), None);
    assert_eq!(s.next_deadline(), Some(251_000));
}

#[test]
fn pacing_does_not_extend_expiry_or_invent_a_wakeup_after_it() {
    let mut s = sender(policy(1_000_000));
    populate(&mut s, 1, 2);
    s.queue_repair(&request(1, 0, 2, 2), 20_000).unwrap();
    let old = s
        .next_repair_packet(20_000, &mut [0; 1_150])
        .unwrap()
        .unwrap();
    assert_eq!(repair(&mut s, 20_001), None);
    assert_eq!(s.next_deadline(), Some(251_000));
    assert_eq!(repair(&mut s, 251_000), None);
    assert_eq!(
        s.authorize_write(&old, 251_000),
        Err(SendError::RepairExpired)
    );
    assert!(!s.needs_recovery()); // No unissued original was silently discarded.
    assert_eq!(s.cached_pictures(), 1);
    assert_eq!(s.next_deadline(), Some(2_000_000));
}

#[test]
fn unfinished_original_expiry_still_fences_every_paced_repair() {
    let mut s = sender(policy(1_000_000));
    populate(&mut s, 1, 2);
    push(&mut s, 2, 1, 1_000).unwrap(); // deliberately do not emit its original
    s.queue_repair(&request(1, 0, 2, 2), 20_000).unwrap();
    assert_eq!(repair(&mut s, 20_000), Some((1, 0)));
    assert_eq!(repair(&mut s, 20_001), None);
    assert_eq!(
        s.next_repair_packet(251_000, &mut [0; 1_150]),
        Err(SendError::OriginalExpired)
    );
    assert!(s.needs_recovery());
    assert_eq!(s.cached_bytes(), 0);
    assert_eq!(
        s.next_repair_packet(251_001, &mut [0; 1_150]),
        Err(SendError::NeedsRecovery)
    );
}

#[test]
fn impossible_packet_is_refused_before_spending_a_round_but_a_short_tail_fits() {
    let c = config();
    let mut s = sender(SendPolicy {
        repair_bytes_per_window: 100,
        ..policy(50_000)
    });
    let length = c.limits.fragment_stride() + 10;
    let descriptor = FrameDescriptor {
        frame: 1,
        reference: Some(0),
        total_bytes: length,
        stride: c.limits.fragment_stride(),
        capture_micros: 1_000,
    };
    s.push(
        Progress {
            descriptor,
            observed_micros: 1_000,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        vec![7; length as usize],
        DeliveryMode::Datagrams,
        1_000,
    )
    .unwrap();
    originals(&mut s, None, 1_000, None);
    assert_eq!(
        s.queue_repair(&request(1, 0, 1, 2), 20_000),
        Err(SendError::RepairBudgetExceeded)
    );
    // The only admitted round remains for the 10-byte tail plus 73-byte header.
    s.queue_repair(&request(1, 1, 2, 2), 20_000).unwrap();
    let offer = s
        .next_repair_packet(20_000, &mut [0; 1_150])
        .unwrap()
        .unwrap();
    assert_eq!(offer.byte_len(), 83);
    assert_eq!(s.next_deadline(), Some(251_000));
}

#[test]
fn backwards_clock_and_close_cannot_turn_waiting_into_refilled_credit() {
    let mut s = sender(policy(50_000));
    populate(&mut s, 1, 2);
    s.queue_repair(&request(1, 0, 2, 2), 20_000).unwrap();
    assert_eq!(repair(&mut s, 20_000), Some((1, 0)));
    assert_eq!(repair(&mut s, 20_001), None);
    assert_eq!(
        s.next_repair_packet(20_000, &mut [0; 1_150]),
        Err(SendError::Delivery(DeliveryError::ClockRegression))
    );
    assert_eq!(s.next_deadline(), None);
    assert_eq!(s.cached_bytes(), 0);
    assert_eq!(
        s.next_repair_packet(50_000, &mut [0; 1_150]),
        Err(SendError::Closed)
    );
}

#[test]
fn four_dependent_pictures_reconstruct_through_window_refills_without_new_requests() {
    let c = config();
    let mut s = SendCache::new(c.limits, c.bindings, c.epoch, policy(50_000)).unwrap();
    let mut r = ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    r.decoder_configured(0).unwrap();
    push(&mut s, 0, 1, 0).unwrap();
    originals(&mut s, Some(&mut r), 0, None);
    let boot = r.take_decodable(0).unwrap().unwrap();
    r.acknowledge_decode(&boot, true, 0).unwrap();
    drop(boot);
    for frame in 1..=4 {
        push(&mut s, frame, 3, 1_000).unwrap();
        originals(&mut s, Some(&mut r), 1_000, Some(0));
    }
    let mut packet = [0; 1_150];
    for frame in 1..=4 {
        let request = r.repair_offer(21_000, &mut packet).unwrap().unwrap();
        assert_eq!(request.frame, frame);
        s.queue_repair(&packet[..request.bytes], 41_000).unwrap();
    }
    // One datagram per window: the requests for later references stay in
    // their admitted slots and survive each temporary byte-allowance shortage.
    for (frame, now) in [(1, 41_000), (2, 50_000), (3, 100_000), (4, 150_000)] {
        let offer = s.next_repair_packet(now, &mut packet).unwrap().unwrap();
        assert_eq!(offer.frame(), frame);
        assert_eq!(offer.send_by_micros(), 251_000);
        s.authorize_write(&offer, now).unwrap();
        r.receive(Channel::Video, &packet[..offer.byte_len()], now + 20_000)
            .unwrap();
        let decoded = r.take_decodable(now + 20_000).unwrap().unwrap();
        assert_eq!(decoded.descriptor().frame, frame);
        assert_eq!(decoded.bytes(), payload(frame, 3));
        assert_eq!(decoded.epoch(), c.epoch);
        r.acknowledge_decode(&decoded, true, now + 20_000).unwrap();
        assert_eq!(s.next_repair_packet(now, &mut packet), Ok(None));
    }
    assert_eq!(r.budget_usage().pictures, 0);
    assert!(!s.needs_recovery());
    assert_eq!(r.repair_offer(171_000, &mut packet), Ok(None));
}

#[test]
fn missed_windows_do_not_accumulate_a_catchup_burst() {
    let mut s = sender(policy(50_000));
    populate(&mut s, 1, 3);
    s.queue_repair(&request(1, 0, 3, 3), 20_000).unwrap();
    assert_eq!(repair(&mut s, 20_000), Some((1, 0)));
    // Three refills elapsed without service. Only the current window is
    // credited; this never banks idle windows or forgives an expired original.
    assert_eq!(repair(&mut s, 200_000), Some((1, 1)));
    assert_eq!(repair(&mut s, 200_000), None);
    assert_eq!(s.next_deadline(), Some(250_000));
    assert_eq!(repair(&mut s, 250_000), Some((1, 2)));
    assert_eq!(s.next_deadline(), Some(251_000));
}
