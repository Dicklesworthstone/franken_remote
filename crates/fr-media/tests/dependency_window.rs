//! Dependency-window derivation is receiver-local and uses only admitted
//! frame rate, negotiated protocol limits and the existing reference horizon.
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_media::delivery::{
    DeliveryError, MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig, ReceivePipeline,
    ReceivePolicy,
};
use fr_wire::{Fragment, MediaLimits, encode_fragment, Channel};

fn config(window: u8, horizon: u64) -> ReceiveConfig {
    let protocol = ProtocolLimits::with_overrides(LimitOverrides {
        reassembly_window_pictures: Some(window),
        ..LimitOverrides::default()
    })
    .unwrap();
    ReceiveConfig {
        limits: MediaLimits::new(protocol, 1_150, 16_384, 64).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy {
            reference_budget_micros: horizon,
            ..ReceivePolicy::default()
        },
    }
}

fn receiver(c: ReceiveConfig) -> ReceivePipeline {
    ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap()
}

#[test]
fn frame_rate_and_reference_horizon_derive_the_active_window() {
    let mut r = receiver(config(12, 250_000));
    assert_eq!(r.configure_frame_rate(1), Ok(3));
    assert_eq!(r.dependency_window_pictures(), 3);

    let mut r = receiver(config(12, 250_000));
    assert_eq!(r.configure_frame_rate(30), Ok(10));

    let mut r = receiver(config(12, 250_000));
    assert_eq!(r.configure_frame_rate(60), Ok(12));

    let mut r = receiver(config(12, 120_000));
    assert_eq!(r.configure_frame_rate(60), Ok(10));

    let mut r = receiver(config(4, 250_000));
    assert_eq!(r.configure_frame_rate(240), Ok(4));
}

#[test]
fn window_configuration_is_fail_closed_and_cannot_change_after_decoder_setup() {
    let c = config(12, 250_000);
    let mut r = receiver(c);
    assert_eq!(r.configure_frame_rate(0), Err(DeliveryError::InvalidPolicy));
    assert_eq!(r.configure_frame_rate(241), Err(DeliveryError::InvalidPolicy));
    assert_eq!(r.configure_frame_rate(30), Ok(10));
    r.decoder_configured(0).unwrap();
    assert_eq!(r.configure_frame_rate(1), Err(DeliveryError::WrongState));
    assert_eq!(r.dependency_window_pictures(), 10);
}

#[test]
fn active_window_not_static_ceiling_bounds_incomplete_picture_metadata() {
    let c = config(12, 250_000);
    let budget = MediaBudget::new(c.limits.protocol()).unwrap();
    let mut r = ReceivePipeline::new(c, budget.clone()).unwrap();
    assert_eq!(r.configure_frame_rate(1), Ok(3));
    r.decoder_configured(0).unwrap();

    // Startup recovery needs a complete independent picture before streaming.
    let recovery_bytes = [7_u8; 16];
    let mut recovery_packet = [0_u8; 1_150];
    let n = fr_wire::encode_recovery(
        fr_wire::RecoveryChunk {
            frame: 0,
            total_bytes: 16,
            offset: 0,
            capture_micros: 0,
            bytes: &recovery_bytes,
        },
        c.bindings.for_channel(Channel::Recovery),
        &c.limits,
        &mut recovery_packet,
    )
    .unwrap();
    r.receive(Channel::Recovery, &recovery_packet[..n], 0).unwrap();
    let picture = r.take_decodable(0).unwrap().unwrap();
    r.acknowledge_decode(&picture, true, 0).unwrap();
    drop(picture);

    for frame in 1_u64..=4 {
        let descriptor = fr_wire::FrameDescriptor {
            frame,
            reference: Some(0),
            total_bytes: 2_500,
            stride: 1_077,
            capture_micros: frame,
        };
        let payload = vec![u8::try_from(frame).unwrap(); 2_500];
        let fragment = Fragment {
            descriptor,
            index: 0,
            bytes: &payload[..1_077],
        };
        let mut packet = [0_u8; 1_150];
        let n = encode_fragment(
            fragment,
            c.bindings.for_channel(Channel::Video),
            &c.limits,
            &mut packet,
        )
        .unwrap();
        if frame <= 3 {
            r.receive(Channel::Video, &packet[..n], frame).unwrap();
            assert_eq!(budget.usage().pictures, usize::try_from(frame).unwrap());
        } else {
            assert_eq!(
                r.receive(Channel::Video, &packet[..n], frame),
                Err(DeliveryError::ResourceLimit)
            );
            // The normal receiver failure path fences and releases partial work.
            assert_eq!(budget.usage().pictures, 0);
        }
    }
}
