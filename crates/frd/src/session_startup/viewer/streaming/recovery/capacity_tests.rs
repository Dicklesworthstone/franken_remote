//! Exercise the native receive boundary with real FRD0 parsing/reassembly and
//! synthetic media. These are not network or native-decoder qualification tests.
use super::{receive, recoverable};
use crate::{native_connection::reconnect, session_startup::ObserverError};
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_media::delivery::{
    BudgetUsage, DeliveryError, MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig,
    ReceivePipeline, ReceivePolicy, ReceiveState,
};
use fr_wire::{
    Channel, FrameDescriptor, MediaLimits, PipelineState, Progress, RecoveryChunk,
    SourceObservation, encode_progress, encode_recovery,
};

fn fixture(decoded: bool) -> (ReceiveConfig, ReceivePipeline, MediaBudget) {
    let protocol = ProtocolLimits::with_overrides(LimitOverrides {
        reassembly_window_pictures: Some(2),
        ..LimitOverrides::default()
    })
    .unwrap();
    let config = ReceiveConfig {
        limits: MediaLimits::new(protocol, 1_150, 16_384, 64).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy::default(),
    };
    let budget = MediaBudget::new(&protocol).unwrap();
    let mut receiver = ReceivePipeline::new(config, budget.clone()).unwrap();
    receiver.decoder_configured(0).unwrap();
    let mut bytes = [0; 1_150];
    let n = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 16,
            offset: 0,
            capture_micros: 0,
            bytes: &[7; 16],
        },
        config.bindings.for_channel(Channel::Recovery),
        &config.limits,
        &mut bytes,
    )
    .unwrap();
    receive(&mut receiver, Channel::Recovery, &bytes[..n], 0, true).unwrap();
    if decoded {
        let idr = receiver.take_decodable(0).unwrap().unwrap();
        receiver.acknowledge_decode(&idr, true, 0).unwrap();
    }
    (config, receiver, budget)
}

fn announcement(config: ReceiveConfig, frame: u64) -> Vec<u8> {
    let mut bytes = vec![0; config.limits.record_bytes()];
    let n = encode_progress(
        Progress {
            descriptor: FrameDescriptor {
                frame,
                reference: Some(frame - 1),
                total_bytes: 16,
                stride: config.limits.fragment_stride(),
                capture_micros: 0,
            },
            observed_micros: 0,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        config.bindings.for_channel(Channel::MediaConfig),
        &config.limits,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}

#[test]
fn native_capacity_recovery_requires_positive_negotiation() {
    for enabled in [false, true] {
        let (config, mut receiver, budget) = fixture(true);
        for frame in 1..=2 {
            receive(
                &mut receiver,
                Channel::MediaConfig,
                &announcement(config, frame),
                frame,
                enabled,
            )
            .unwrap();
        }
        let result = receive(
            &mut receiver,
            Channel::MediaConfig,
            &announcement(config, 3),
            3,
            enabled,
        );
        assert_eq!(
            result,
            if enabled {
                Ok(())
            } else {
                Err(DeliveryError::ReferenceCapacityExceeded)
            }
        );
        assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
        assert_eq!(
            receiver.failure(),
            Some(DeliveryError::ReferenceCapacityExceeded)
        );
        assert_eq!(budget.usage(), BudgetUsage::default());
        assert!(receiver.latest_progress().is_none());
    }
}

#[test]
fn resource_and_protocol_failures_are_not_added_to_the_recovery_allowlist() {
    for error in [
        DeliveryError::ResourceLimit,
        DeliveryError::AllocationFailed,
        DeliveryError::ConflictingPicture,
        DeliveryError::NoncontiguousRecovery,
        DeliveryError::ClockRegression,
        DeliveryError::ClockOverflow,
        DeliveryError::StaleGeneration,
        DeliveryError::DecodeMismatch,
        DeliveryError::WrongState,
        DeliveryError::InvalidPolicy,
        DeliveryError::Wire(fr_wire::WireError::InvalidRanges),
    ] {
        assert!(
            !recoverable(error),
            "terminal error was admitted: {error:?}"
        );
    }
    for error in [
        DeliveryError::ReferenceCapacityExceeded,
        DeliveryError::ReferenceExpired,
        DeliveryError::RecoveryExpired,
        DeliveryError::DecodeFailed,
    ] {
        assert!(recoverable(error));
    }
}

#[test]
fn native_startup_capacity_refusal_stays_terminal_despite_recovery_capability() {
    let (config, mut receiver, budget) = fixture(false);
    receive(
        &mut receiver,
        Channel::MediaConfig,
        &announcement(config, 1),
        1,
        true,
    )
    .unwrap();
    assert_eq!(budget.usage().pictures, 2);
    assert_eq!(
        receive(
            &mut receiver,
            Channel::MediaConfig,
            &announcement(config, 2),
            2,
            true,
        ),
        Err(DeliveryError::ResourceLimit)
    );
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn full_native_window_does_not_turn_malformed_traffic_into_recovery() {
    let (config, mut receiver, budget) = fixture(true);
    for frame in 1..=2 {
        receive(
            &mut receiver,
            Channel::MediaConfig,
            &announcement(config, frame),
            frame,
            true,
        )
        .unwrap();
    }
    let mut invalid = announcement(config, 3);
    assert!(invalid.pop().is_some());
    assert!(matches!(
        receive(&mut receiver, Channel::MediaConfig, &invalid, 3, true),
        Err(DeliveryError::Wire(_))
    ));
    assert_eq!(budget.usage(), BudgetUsage::default());
}

fn observation(error: super::Error) -> reconnect::Failure {
    reconnect::Failure::Observation(ObserverError::Streaming(error))
}

#[test]
fn actual_reference_expiry_retains_fresh_view_fallback_through_reporting() {
    let (config, mut receiver, budget) = fixture(true);
    receive(
        &mut receiver,
        Channel::MediaConfig,
        &announcement(config, 1),
        1,
        true,
    )
    .unwrap();
    let until = receiver.reference_deadline().unwrap();
    assert_eq!(until, 1 + config.policy.reference_budget_micros);
    assert_eq!(receiver.tick(until - 1), Ok(()));
    assert_eq!(receiver.failure(), None);
    assert_eq!(receiver.tick(until), Err(DeliveryError::ReferenceExpired));
    assert_eq!(receiver.failure(), Some(DeliveryError::ReferenceExpired));
    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
    assert_eq!(budget.usage(), BudgetUsage::default());

    // The actual receiver has failed at its ORIGINAL boundary. These two
    // explicit service-outcome fixtures exercise the production translation,
    // not a network recovery or a claim that a replacement decoder started.
    for horizon in [DeliveryError::ReferenceExpired, DeliveryError::RecoveryExpired] {
        let wrapped = super::recovery_control::Error::Delivery(horizon);
        let mapped = super::service_failure(wrapped, receiver.failure());
        assert_eq!(mapped, super::Error::Delivery(horizon));
        assert!(reconnect::retryable(observation(mapped)));
        assert_eq!(
            reconnect::lapse(observation(mapped)),
            Some(reconnect::Lapse::VideoReferenceLost)
        );
        // An arbitrary wrapper outside this provenance-checked producer does
        // NOT become a retry instruction. Neither path revives the old view.
        assert!(!reconnect::retryable(observation(super::Error::Recovery(wrapped))));
        assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
        assert!(receiver.latest_progress().is_none());
        assert_eq!(receiver.reference_deadline(), None);
        assert_eq!(receiver.tick(until + 1), Err(DeliveryError::ReferenceExpired));
    }
}

#[test]
fn a_failed_decode_cannot_be_relabelled_by_a_later_recovery_timeout() {
    let (_, mut receiver, budget) = fixture(false);
    let idr = receiver.take_decodable(0).unwrap().unwrap();
    assert_eq!(
        receiver.acknowledge_decode(&idr, false, 0).err(),
        Some(DeliveryError::DecodeFailed)
    );
    drop(idr);
    assert_eq!(receiver.failure(), Some(DeliveryError::DecodeFailed));
    let wrapped = super::recovery_control::Error::Delivery(DeliveryError::RecoveryExpired);
    let mapped = super::service_failure(wrapped, receiver.failure());
    assert_eq!(mapped, super::Error::Recovery(wrapped));
    assert!(!reconnect::retryable(observation(mapped)));
    assert_eq!(reconnect::lapse(observation(mapped)), None);
    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn recovery_timeout_needs_the_original_receivers_expiry_not_just_an_error_code() {
    for original in [
        None,
        Some(DeliveryError::DecodeFailed),
        Some(DeliveryError::ReferenceCapacityExceeded),
        Some(DeliveryError::ResourceLimit),
        Some(DeliveryError::AllocationFailed),
        Some(DeliveryError::ClockRegression),
        Some(DeliveryError::DecodeMismatch),
    ] {
        for horizon in [DeliveryError::ReferenceExpired, DeliveryError::RecoveryExpired] {
            let wrapped = super::recovery_control::Error::Delivery(horizon);
            let mapped = super::service_failure(wrapped, original);
            assert_eq!(mapped, super::Error::Recovery(wrapped));
            assert!(!reconnect::retryable(observation(mapped)), "{original:?}");
        }
    }
    // Both expired horizons are allowed when genuinely retained by the same
    // original receiver. The current service's precise error is preserved.
    for original in [DeliveryError::ReferenceExpired, DeliveryError::RecoveryExpired] {
        for current in [DeliveryError::ReferenceExpired, DeliveryError::RecoveryExpired] {
            assert_eq!(
                super::service_failure(
                    super::recovery_control::Error::Delivery(current),
                    Some(original),
                ),
                super::Error::Delivery(current)
            );
        }
    }
}

#[test]
fn a_prior_expiry_never_masks_current_authority_protocol_or_scope_refusal() {
    use super::recovery_control::Error as Recovery;
    use fr_transport::quic::Error as Transport;

    for error in [
        Recovery::WrongBinding,
        Recovery::Closed,
        Recovery::NotNegotiated,
        Recovery::Transport(Transport::Unauthorized),
        Recovery::Transport(Transport::Cancelled),
        Recovery::Transport(Transport::Malformed),
        Recovery::Transport(Transport::Closed),
        Recovery::Transport(Transport::Clock),
        Recovery::Delivery(DeliveryError::DecodeFailed),
        Recovery::Delivery(DeliveryError::ReferenceCapacityExceeded),
        Recovery::Delivery(DeliveryError::ClockRegression),
        Recovery::Delivery(DeliveryError::StaleGeneration),
        Recovery::Delivery(DeliveryError::DecodeMismatch),
    ] {
        let mapped = super::service_failure(error, Some(DeliveryError::ReferenceExpired));
        assert_eq!(mapped, super::Error::Recovery(error));
        assert!(!reconnect::retryable(observation(mapped)), "{error:?}");
    }
}
