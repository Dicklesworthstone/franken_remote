//! Independent pictures are recovery candidates, not decode or visibility
//! receipts. Only queued work may be superseded; decoder ownership stays live.
use super::*;

fn complete(receiver: &mut ReceivePipeline, c: ReceiveConfig, d: FrameDescriptor, now: u64) {
    for packet in fragments(d, &payload(), c) {
        receiver.receive(Channel::Video, &packet, now).unwrap();
    }
}

#[test]
fn newest_complete_independent_picture_supersedes_older_ready_chain() {
    let c = config();
    let (mut receiver, budget) = running(c);
    complete(&mut receiver, c, descriptor(1, Some(0)), 1);
    complete(&mut receiver, c, descriptor(2, Some(1)), 2);
    complete(&mut receiver, c, descriptor(3, None), 3);
    complete(&mut receiver, c, descriptor(4, Some(3)), 4);
    complete(&mut receiver, c, descriptor(5, None), 5);
    complete(&mut receiver, c, descriptor(6, Some(5)), 6);

    let independent = receiver.take_decodable(7).unwrap().unwrap();
    assert_eq!(independent.descriptor().frame, 5);
    assert_eq!(budget.usage().pictures, 2);
    receiver.acknowledge_decode(&independent, true, 8).unwrap();
    drop(independent);
    let dependent = receiver.take_decodable(9).unwrap().unwrap();
    assert_eq!(dependent.descriptor().frame, 6);
    receiver.acknowledge_decode(&dependent, true, 10).unwrap();
    drop(dependent);
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn superseded_incomplete_picture_cannot_expire_a_complete_recovery_candidate() {
    let c = config();
    let (mut receiver, _) = running(c);
    let old = fragments(descriptor(1, Some(0)), &payload(), c);
    receiver.receive(Channel::Video, &old[0], 1).unwrap();
    complete(&mut receiver, c, descriptor(3, None), 200_000);

    // The old reference has now expired. The independent candidate completed
    // before that expiry and owns its original, later reference deadline.
    let now = 1 + c.policy.reference_budget_micros;
    receiver.tick(now).unwrap();
    let picture = receiver.take_decodable(now).unwrap().unwrap();
    assert_eq!(picture.descriptor().frame, 3);
    assert_eq!(picture.reference_deadline_us(), 450_000);
    assert_eq!(picture.display_deadline_us(), 250_000);
    assert!(!picture.within_display_queue_budget());
    receiver.acknowledge_decode(&picture, true, now).unwrap();
}

#[test]
fn late_superseded_packets_and_pending_repairs_cannot_repopulate_the_queue() {
    let c = config();
    let (mut receiver, budget) = running(c);
    let old = descriptor(1, Some(0));
    let packets = fragments(old, &payload(), c);
    receiver.receive(Channel::Video, &packets[0], 1).unwrap();
    let mut bytes = [0; 1_150];
    let repair = receiver.repair_offer(20_001, &mut bytes).unwrap().unwrap();
    assert!(receiver.repair_needed(repair.frame));
    complete(&mut receiver, c, descriptor(3, None), 100_000);
    let retained = budget.usage();
    assert_eq!(retained.pictures, 1);
    assert!(!receiver.repair_needed(repair.frame));
    for packet in packets {
        assert_eq!(
            receiver.receive(Channel::Video, &packet, 100_001),
            Ok(ReceiveUpdate::Obsolete)
        );
    }
    assert_eq!(
        receiver.receive(Channel::MediaConfig, &progress(old, c), 100_001),
        Ok(ReceiveUpdate::Obsolete)
    );
    assert_eq!(budget.usage(), retained);
    assert_eq!(receiver.reference_deadline(), Some(350_000));
    assert!(
        receiver
            .repair_offer(100_001, &mut bytes)
            .unwrap()
            .is_none()
    );
}

#[test]
fn independent_completion_cannot_preempt_decoder_ownership_or_release_its_budget() {
    let c = config();
    let (mut receiver, budget) = running(c);
    complete(&mut receiver, c, descriptor(1, Some(0)), 1);
    let decoding = receiver.take_decodable(2).unwrap().unwrap();
    let held = budget.usage();
    let old = fragments(descriptor(2, Some(1)), &payload(), c);
    receiver.receive(Channel::Video, &old[0], 2).unwrap();
    complete(&mut receiver, c, descriptor(5, None), 3);
    assert!(decoding.is_live());
    assert!(receiver.take_decodable(4).unwrap().is_none());
    assert_eq!(budget.usage().pictures, 2);
    assert_eq!(receiver.reference_deadline(), Some(250_001));
    receiver.acknowledge_decode(&decoding, true, 5).unwrap();
    assert_eq!(budget.usage().pictures, 2);
    let candidate = receiver.take_decodable(6).unwrap().unwrap();
    assert_eq!(candidate.descriptor().frame, 5);
    assert_eq!(candidate.reference_deadline_us(), 250_003);
    drop(decoding);
    assert_eq!(budget.usage(), held);
    receiver.acknowledge_decode(&candidate, true, 7).unwrap();
    drop(candidate);
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn complete_independent_candidate_cannot_waive_a_stuck_decoder_deadline() {
    let c = config();
    let (mut receiver, budget) = running(c);
    complete(&mut receiver, c, descriptor(1, Some(0)), 1);
    let decoding = receiver.take_decodable(2).unwrap().unwrap();
    let held = budget.usage();
    complete(&mut receiver, c, descriptor(5, None), 100_000);
    assert_eq!(receiver.tick(250_001), Err(DeliveryError::ReferenceExpired));
    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
    assert!(!decoding.is_live());
    assert_eq!(budget.usage(), held);
    drop(decoding);
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn incomplete_independent_picture_does_not_supersede_the_working_chain() {
    let c = config();
    let (mut receiver, budget) = running(c);
    complete(&mut receiver, c, descriptor(1, Some(0)), 1);
    let newer = fragments(descriptor(5, None), &payload(), c);
    receiver.receive(Channel::Video, &newer[0], 2).unwrap();
    let picture = receiver.take_decodable(3).unwrap().unwrap();
    assert_eq!(picture.descriptor().frame, 1);
    receiver.acknowledge_decode(&picture, true, 4).unwrap();
    drop(picture);
    assert_eq!(budget.usage().pictures, 1);
    assert_eq!(receiver.tick(250_002), Err(DeliveryError::ReferenceExpired));
}

#[test]
fn invalid_independent_decode_still_fences_the_epoch_and_retains_owned_bytes() {
    let c = config();
    let (mut receiver, budget) = running(c);
    complete(&mut receiver, c, descriptor(1, Some(0)), 1);
    complete(&mut receiver, c, descriptor(5, None), 2);
    let picture = receiver.take_decodable(3).unwrap().unwrap();
    assert_eq!(picture.descriptor().frame, 5);
    let held = budget.usage();
    assert_eq!(
        receiver.acknowledge_decode(&picture, false, 4),
        Err(DeliveryError::DecodeFailed)
    );
    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
    assert!(!picture.is_live());
    assert_eq!(budget.usage(), held);
    drop(picture);
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn candidate_completion_at_an_expired_reference_does_not_resurrect_the_chain() {
    let c = config();
    let (mut receiver, _) = running(c);
    let old = fragments(descriptor(1, Some(0)), &payload(), c);
    receiver.receive(Channel::Video, &old[0], 1).unwrap();
    let candidate = fragments(descriptor(5, None), &payload(), c);
    for packet in &candidate[..candidate.len() - 1] {
        receiver.receive(Channel::Video, packet, 200_000).unwrap();
    }
    assert_eq!(
        receiver.receive(Channel::Video, candidate.last().unwrap(), 250_001),
        Err(DeliveryError::ReferenceExpired)
    );
    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
}

#[test]
fn queued_independent_picture_never_bypasses_reliable_startup_decode() {
    let c = config();
    let mut receiver =
        ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    receiver.decoder_configured(0).unwrap();
    recovery(&mut receiver, c, 0);
    complete(&mut receiver, c, descriptor(1, Some(0)), 1);
    complete(&mut receiver, c, descriptor(5, None), 1);
    let startup = receiver.take_decodable(2).unwrap().unwrap();
    assert_eq!(startup.descriptor().frame, 0);
    assert_eq!(receiver.state(), ReceiveState::DecodingRecovery);
    assert!(receiver.take_decodable(2).unwrap().is_none());
    receiver.acknowledge_decode(&startup, true, 3).unwrap();
    let candidate = receiver.take_decodable(4).unwrap().unwrap();
    assert_eq!(candidate.descriptor().frame, 5);
}
