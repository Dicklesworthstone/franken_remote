//! Exercise the native receive boundary with real FRD0 parsing/reassembly and
//! synthetic media. These are not network or native-decoder qualification tests.
use super::{receive, recoverable};
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
        assert!(!recoverable(error), "terminal error was admitted: {error:?}");
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
