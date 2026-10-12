//! Production wire/reassembly/recovery owners with synthetic compressed bytes.
//! No native HEVC, live-tailnet, timing or input-execution claim is made here.
use super::{config, descriptor, fragments, payload, progress, recovery};
use fr_core::{
    ids::{
        DisplayGeometryGeneration, HostBootId, OsSessionId, RemoteSessionId,
        ViewportMappingGeneration,
    },
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_media::delivery::{
    BudgetUsage, DecoderBinding, DeliveryError, MediaBindings, MediaBudget, MediaEpoch,
    ReceiveConfig, ReceivePipeline, ReceiveState, RecoveryRequestor,
};
use fr_wire::{
    Channel, MediaLimits,
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    recovery_request::{self, Reason},
};

fn bounded(window: u8, bytes: Option<u32>) -> ReceiveConfig {
    let protocol = ProtocolLimits::with_overrides(LimitOverrides {
        reassembly_window_pictures: Some(window),
        max_encoded_access_unit_bytes: bytes,
        per_viewer_compressed_bytes: bytes.map(u64::from),
        ..LimitOverrides::default()
    })
    .unwrap();
    ReceiveConfig {
        limits: MediaLimits::new(protocol, 1_150, 16_384, 64).unwrap(),
        ..config()
    }
}

fn binding(c: ReceiveConfig) -> Binding {
    Binding {
        parent: ControlBinding {
            id: 1_000,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        },
        display: 4,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: c.epoch.configuration,
        recovery: c.epoch.recovery,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}

fn ready(
    c: ReceiveConfig,
) -> (
    ReceivePipeline,
    MediaBudget,
    DecoderBinding,
    RecoveryRequestor,
) {
    let budget = MediaBudget::new(c.limits.protocol()).unwrap();
    let mut receiver = ReceivePipeline::new(c, budget.clone()).unwrap();
    let decoder = receiver
        .bind_decoder(c.epoch.configuration, c.limits.protocol(), 0)
        .unwrap();
    let mut requestor = RecoveryRequestor::new(&receiver, binding(c)).unwrap();
    recovery(&mut receiver, c, 0);
    let idr = receiver.take_decodable(0).unwrap().unwrap();
    let completed = receiver.complete_decode(&idr, 0).unwrap();
    requestor.observe_decoded(&completed).unwrap();
    drop(idr);
    (receiver, budget, decoder, requestor)
}

fn announce(c: ReceiveConfig, frame: u64, size: u32) -> Vec<u8> {
    let mut d = descriptor(frame, Some(frame - 1));
    d.total_bytes = size;
    progress(d, c)
}

fn fill(receiver: &mut ReceivePipeline, c: ReceiveConfig) {
    for frame in 1..=2 {
        receiver
            .receive(Channel::MediaConfig, &announce(c, frame, 16), frame)
            .unwrap();
    }
}

#[test]
fn slot_pressure_requests_one_bounded_recovery_on_the_original_receiver() {
    let c = bounded(2, None);
    let (mut receiver, budget, decoder, mut requestor) = ready(c);
    fill(&mut receiver, c);
    assert_eq!(
        receiver.receive(Channel::MediaConfig, &announce(c, 3, 16), 3),
        Err(DeliveryError::ReferenceCapacityExceeded)
    );
    assert_eq!(
        receiver.failure(),
        Some(DeliveryError::ReferenceCapacityExceeded)
    );
    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
    assert_eq!(budget.usage(), BudgetUsage::default());
    assert_eq!(decoder.check(&receiver), Err(DeliveryError::DecodeMismatch));
    decoder.check_recovery(&receiver).unwrap();
    assert!(receiver.latest_progress().is_none());
    assert_eq!(receiver.next_deadline(), None);

    let mut bytes = [0; recovery_request::REQUEST_BYTES];
    let offer = requestor
        .offer(&mut receiver, 3, &mut bytes)
        .unwrap()
        .unwrap();
    assert_eq!(offer.reason(), Reason::ReferenceExpired);
    let until = offer.send_by_micros();
    assert_eq!(until, 3 + c.policy.recovery_budget_micros);
    let request = recovery_request::decode(
        &bytes[..offer.byte_len()],
        binding(c),
        c.limits.protocol(),
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(request.reason, Reason::ReferenceExpired);
    assert_eq!(request.last_useful_frame, Some(0));
    let original = bytes;
    for now in 4..=8 {
        let again = requestor
            .offer(&mut receiver, now, &mut bytes)
            .unwrap()
            .unwrap();
        assert_eq!(again.send_by_micros(), until);
        assert_eq!(bytes, original);
    }
    requestor.mark_sent(&receiver, &offer, 9).unwrap();
    assert!(
        requestor
            .offer(&mut receiver, 10, &mut bytes)
            .unwrap()
            .is_none()
    );
    assert_eq!(requestor.next_deadline(), Some(until));
}

#[test]
fn replacement_reuses_the_original_budget_and_fences_the_old_decoder() {
    let c = bounded(2, None);
    let (mut receiver, budget, decoder, mut requestor) = ready(c);
    fill(&mut receiver, c);
    assert_eq!(
        receiver.receive(Channel::MediaConfig, &announce(c, 3, 16), 3),
        Err(DeliveryError::ReferenceCapacityExceeded)
    );
    let mut bytes = [0; recovery_request::REQUEST_BYTES];
    let offer = requestor
        .offer(&mut receiver, 4, &mut bytes)
        .unwrap()
        .unwrap();
    let until = offer.send_by_micros();
    requestor.mark_sent(&receiver, &offer, 5).unwrap();
    let next = ReceiveConfig {
        epoch: MediaEpoch {
            recovery: c.epoch.recovery.next().unwrap(),
            ..c.epoch
        },
        bindings: MediaBindings::new(5, 6, 7, 8).unwrap(),
        ..c
    };
    assert_eq!(
        requestor.authorize_replacement(&receiver, binding(next), next.limits, next.bindings, 11),
        Ok(until)
    );
    receiver.replace(next.epoch, next.bindings, 11).unwrap();
    assert_eq!(
        decoder.check_recovery(&receiver),
        Err(DeliveryError::DecodeMismatch)
    );
    assert!(matches!(
        requestor.offer(&mut receiver, 11, &mut bytes),
        Err(DeliveryError::StaleGeneration)
    ));
    receiver
        .bind_decoder(next.epoch.configuration, next.limits.protocol(), 11)
        .unwrap();
    recovery(&mut receiver, next, 12);
    let idr = receiver.take_decodable(12).unwrap().unwrap();
    receiver.complete_decode(&idr, 12).unwrap();
    drop(idr);
    assert_eq!(receiver.state(), ReceiveState::Streaming);
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn aggregate_byte_pressure_recovers_before_the_picture_count_ceiling() {
    let c = bounded(4, Some(4_096));
    let (mut receiver, budget, _, mut requestor) = ready(c);
    receiver
        .receive(Channel::MediaConfig, &announce(c, 1, 2_500), 1)
        .unwrap();
    assert_eq!(budget.usage().pictures, 1);
    assert!(budget.usage().bytes > 2_500);
    assert!(budget.usage().bytes <= 4_096);
    assert_eq!(
        receiver.receive(Channel::MediaConfig, &announce(c, 2, 2_500), 2),
        Err(DeliveryError::ReferenceCapacityExceeded)
    );
    assert_eq!(budget.usage(), BudgetUsage::default());
    let offer = requestor
        .offer(&mut receiver, 2, &mut [0; recovery_request::REQUEST_BYTES])
        .unwrap()
        .unwrap();
    assert_eq!(offer.reason(), Reason::ReferenceExpired);
}

#[test]
fn capacity_failure_keeps_decoder_borrow_charged_across_replacement() {
    let c = bounded(2, None);
    let (mut receiver, budget, decoder, mut requestor) = ready(c);
    for packet in fragments(descriptor(1, Some(0)), &payload(), c) {
        receiver.receive(Channel::Video, &packet, 1).unwrap();
    }
    let held = receiver.take_decodable(2).unwrap().unwrap();
    let held_usage = budget.usage();
    decoder.check_picture(&held).unwrap();
    receiver
        .receive(Channel::MediaConfig, &announce(c, 2, 16), 3)
        .unwrap();
    assert_eq!(budget.usage().pictures, 2);
    assert_eq!(
        receiver.receive(Channel::MediaConfig, &announce(c, 3, 16), 4),
        Err(DeliveryError::ReferenceCapacityExceeded)
    );
    assert_eq!(budget.usage(), held_usage);
    assert_eq!(held.bytes(), payload());
    assert!(!held.is_live());
    assert_eq!(
        decoder.check_picture(&held),
        Err(DeliveryError::DecodeMismatch)
    );
    assert!(matches!(
        receiver.complete_decode(&held, 4),
        Err(DeliveryError::StaleGeneration)
    ));

    let offer = requestor
        .offer(&mut receiver, 4, &mut [0; recovery_request::REQUEST_BYTES])
        .unwrap()
        .unwrap();
    requestor.mark_sent(&receiver, &offer, 4).unwrap();
    let next = ReceiveConfig {
        epoch: MediaEpoch {
            recovery: c.epoch.recovery.next().unwrap(),
            ..c.epoch
        },
        bindings: MediaBindings::new(5, 6, 7, 8).unwrap(),
        ..c
    };
    requestor
        .authorize_replacement(&receiver, binding(next), next.limits, next.bindings, 5)
        .unwrap();
    receiver.replace(next.epoch, next.bindings, 5).unwrap();
    assert_eq!(budget.usage(), held_usage);
    let next_decoder = receiver
        .bind_decoder(next.epoch.configuration, next.limits.protocol(), 5)
        .unwrap();
    recovery(&mut receiver, next, 6);
    let idr = receiver.take_decodable(6).unwrap().unwrap();
    receiver.complete_decode(&idr, 6).unwrap();
    assert_eq!(budget.usage().pictures, 2);
    assert_eq!(
        next_decoder.check_picture(&held),
        Err(DeliveryError::DecodeMismatch)
    );
    drop(idr);
    assert_eq!(budget.usage(), held_usage);
    drop(held);
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn impossible_individual_allocation_including_metadata_remains_terminal_even_when_full() {
    for full in [false, true] {
        let c = bounded(2, Some(4_096));
        let (mut receiver, budget, _, mut requestor) = ready(c);
        if full {
            fill(&mut receiver, c);
            assert_eq!(budget.usage().pictures, 2);
        }
        // The declared payload fits max_access_unit, but not with required
        // metadata. A full slot table must not camouflage that impossibility.
        assert_eq!(
            receiver.receive(Channel::MediaConfig, &announce(c, 3, 4_096), 3),
            Err(DeliveryError::ResourceLimit)
        );
        assert!(matches!(
            requestor.offer(&mut receiver, 3, &mut [0; recovery_request::REQUEST_BYTES]),
            Err(DeliveryError::ResourceLimit)
        ));
        assert_eq!(requestor.next_deadline(), None);
        assert_eq!(budget.usage(), BudgetUsage::default());
    }
}

#[test]
fn startup_and_unfinished_recovery_cannot_recursively_recover_capacity_failures() {
    for decoding in [false, true] {
        let c = bounded(2, None);
        let budget = MediaBudget::new(c.limits.protocol()).unwrap();
        let mut receiver = ReceivePipeline::new(c, budget.clone()).unwrap();
        receiver.decoder_configured(0).unwrap();
        let mut requestor = RecoveryRequestor::new(&receiver, binding(c)).unwrap();
        recovery(&mut receiver, c, 0);
        let held = if decoding {
            receiver.take_decodable(0).unwrap()
        } else {
            None
        };
        receiver
            .receive(Channel::MediaConfig, &announce(c, 1, 16), 1)
            .unwrap();
        assert_eq!(budget.usage().pictures, 2);
        assert_eq!(
            receiver.receive(Channel::MediaConfig, &announce(c, 2, 16), 2),
            Err(DeliveryError::ResourceLimit)
        );
        assert!(matches!(
            requestor.offer(&mut receiver, 2, &mut [0; recovery_request::REQUEST_BYTES]),
            Err(DeliveryError::ResourceLimit)
        ));
        assert_eq!(budget.usage().pictures, usize::from(decoding));
        drop(held);
        assert_eq!(budget.usage(), BudgetUsage::default());
    }
}

#[test]
fn full_window_duplicates_and_stale_bindings_preserve_healthy_state() {
    let c = bounded(2, None);
    let (mut receiver, budget, decoder, mut requestor) = ready(c);
    fill(&mut receiver, c);
    let charged = budget.usage();
    for now in 3..=20 {
        receiver
            .receive(Channel::MediaConfig, &announce(c, 2, 16), now)
            .unwrap();
        assert_eq!(budget.usage(), charged);
    }
    let mut stale = announce(c, 3, 16);
    stale[16..20].copy_from_slice(&99_u32.to_be_bytes());
    assert_eq!(
        receiver.receive(Channel::MediaConfig, &stale, 21),
        Err(DeliveryError::StaleGeneration)
    );
    assert_eq!(receiver.state(), ReceiveState::Streaming);
    decoder.check(&receiver).unwrap();
    assert_eq!(budget.usage(), charged);
    assert!(
        requestor
            .offer(&mut receiver, 21, &mut [0; recovery_request::REQUEST_BYTES])
            .unwrap()
            .is_none()
    );
    // Duplicate receipt did not restart either reference deadline.
    assert_eq!(
        receiver.reference_deadline(),
        Some(1 + c.policy.reference_budget_micros)
    );
}

#[test]
fn malformed_and_conflicting_records_remain_terminal_with_a_full_window() {
    for malformed in [false, true] {
        let c = bounded(2, None);
        let (mut receiver, budget, _, mut requestor) = ready(c);
        fill(&mut receiver, c);
        let mut packet = announce(c, 2, 17);
        if malformed {
            assert!(packet.pop().is_some());
        }
        let error = receiver
            .receive(Channel::MediaConfig, &packet, 3)
            .unwrap_err();
        if malformed {
            assert!(matches!(error, DeliveryError::Wire(_)));
        } else {
            assert_eq!(error, DeliveryError::ConflictingPicture);
        }
        assert!(matches!(
            requestor.offer(&mut receiver, 3, &mut [0; recovery_request::REQUEST_BYTES]),
            Err(cause) if cause == error
        ));
        assert_eq!(budget.usage(), BudgetUsage::default());
    }
}

#[test]
fn capacity_recovery_wait_expires_without_a_fresh_budget_or_duplicate_request() {
    let c = bounded(2, None);
    let (mut receiver, _, _, mut requestor) = ready(c);
    fill(&mut receiver, c);
    assert_eq!(
        receiver.receive(Channel::MediaConfig, &announce(c, 3, 16), 3),
        Err(DeliveryError::ReferenceCapacityExceeded)
    );
    // Even an undersized output starts the fixed failure budget without a send.
    assert!(matches!(
        requestor.offer(&mut receiver, 3, &mut []),
        Err(DeliveryError::Wire(_))
    ));
    let until = requestor.next_deadline().unwrap();
    let mut bytes = [0; recovery_request::REQUEST_BYTES];
    let offer = requestor
        .offer(&mut receiver, until - 1, &mut bytes)
        .unwrap()
        .unwrap();
    assert_eq!(offer.send_by_micros(), until);
    assert_eq!(
        requestor.authorize_write(&receiver, &offer, until),
        Err(DeliveryError::RecoveryExpired)
    );
    assert!(
        requestor
            .offer(&mut receiver, until + 1, &mut bytes)
            .is_err()
    );
    assert_eq!(requestor.next_deadline(), None);
    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
}
