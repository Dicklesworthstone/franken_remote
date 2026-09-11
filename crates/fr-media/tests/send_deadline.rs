//! Capture-to-enqueue stalls cannot make encoded pictures young again.
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::{
    DeliveryMode, MediaBindings, MediaEpoch, SendCache, SendError, SendPolicy,
};
use fr_wire::{FrameDescriptor, MediaLimits, PipelineState, Progress, SourceObservation};
fn sender() -> (SendCache, MediaLimits) {
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1_150, 16_384, 64).unwrap();
    (
        SendCache::new(
            limits,
            MediaBindings::new(1, 2, 3, 4).unwrap(),
            MediaEpoch {
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
            SendPolicy::default(),
        )
        .unwrap(),
        limits,
    )
}
fn picture(limits: MediaLimits, frame: u64, capture: u64) -> Progress {
    Progress {
        descriptor: FrameDescriptor {
            frame,
            reference: frame.checked_sub(1),
            total_bytes: 128,
            stride: limits.fragment_stride(),
            capture_micros: capture,
        },
        observed_micros: capture,
        observation: SourceObservation::Captured,
        pipeline: PipelineState::Running,
    }
}
fn drain(tx: &mut SendCache, now: u64, deadline: u64) {
    let mut buffer = [0; 1_150];
    while let Some(packet) = tx.next_packet(now, &mut buffer).unwrap() {
        assert_eq!(packet.send_by_micros(), deadline);
        tx.authorize_write(&packet, now).unwrap();
    }
}
#[test]
fn both_recovery_and_predicted_packets_keep_the_capture_anchored_deadline() {
    let (mut tx, l) = sender();
    tx.push(
        picture(l, 0, 100),
        vec![8; 128],
        DeliveryMode::Recovery,
        80_100,
    )
    .unwrap();
    drain(&mut tx, 80_100, 2_000_100);
    tx.push(
        picture(l, 1, 100_100),
        vec![9; 128],
        DeliveryMode::Datagrams,
        300_099,
    )
    .unwrap();
    drain(&mut tx, 300_099, 350_100);
    tx.tick(350_100).unwrap();
    assert_eq!(
        tx.cached_pictures(),
        1,
        "the old recovery picture retains its own horizon"
    );
}
#[test]
fn expired_picture_is_refused_without_retiring_sequence_or_charging_storage() {
    let (mut tx, l) = sender();
    assert_eq!(
        tx.push(
            picture(l, 0, 1),
            vec![1; 128],
            DeliveryMode::Recovery,
            2_000_001
        ),
        Err(SendError::OriginalExpired)
    );
    assert_eq!((tx.cached_pictures(), tx.cached_bytes()), (0, 0));
    tx.push(
        picture(l, 0, 2_000_001),
        vec![1; 128],
        DeliveryMode::Recovery,
        2_000_001,
    )
    .unwrap();
    drain(&mut tx, 2_000_001, 4_000_001);
    assert_eq!(
        tx.push(
            picture(l, 1, 2_000_001),
            vec![2; 128],
            DeliveryMode::Datagrams,
            2_250_001
        ),
        Err(SendError::OriginalExpired)
    );
    assert_eq!(tx.cached_pictures(), 1);
    tx.push(
        picture(l, 1, 2_250_001),
        vec![2; 128],
        DeliveryMode::Datagrams,
        2_250_001,
    )
    .unwrap();
}
#[test]
fn future_capture_and_overflowing_lifetimes_are_not_admitted() {
    let (mut tx, l) = sender();
    assert_eq!(
        tx.push(picture(l, 0, 2), vec![1; 128], DeliveryMode::Recovery, 1),
        Err(SendError::InvalidObservation)
    );
    assert_eq!((tx.cached_pictures(), tx.cached_bytes()), (0, 0));
    assert!(
        tx.push(
            picture(l, 0, u64::MAX - 1),
            vec![1; 128],
            DeliveryMode::Recovery,
            u64::MAX - 1
        )
        .is_err()
    );
    assert_eq!((tx.cached_pictures(), tx.cached_bytes()), (0, 0));
}

#[test]
fn capture_credit_includes_capacity_and_keeps_packetizer_state_untouched() {
    let (mut tx, l) = sender();
    assert!(tx.can_push_capacity(128));
    assert!(tx.can_push_capacity(tx.maximum_capacity()));
    assert!(!tx.can_push_capacity(tx.maximum_capacity() + 1));
    assert!(tx.maximum_capacity() < SendPolicy::default().max_cached_bytes);
    assert!(!tx.can_push_capacity(usize::MAX));
    assert!(!tx.originals_pending());
    tx.push(picture(l, 0, 0), vec![8; 128], DeliveryMode::Recovery, 0)
        .unwrap();
    assert!(tx.originals_pending());
    for _ in 0..3 {
        assert!(tx.can_push_capacity(128));
        assert!(tx.originals_pending());
    }
    drain(&mut tx, 0, 2_000_000);
    assert!(!tx.originals_pending());
    let maximum = SendPolicy::default().max_cached_bytes;
    assert!(
        !tx.can_push_capacity(maximum - tx.cached_bytes()),
        "metadata also consumes credit"
    );
    tx.observe_unchanged(0, 1, 1).unwrap();
    assert!(tx.originals_pending());
    drain(&mut tx, 1, 250_001);
    assert!(!tx.originals_pending());
    tx.close();
    assert!(!tx.can_push_capacity(1));
}
