//! The Video role shares one admitted binding across typed delivery lanes.
//! These real packetizer/reassembler tests are not native-network qualification.
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::*;
use fr_wire::*;

fn config() -> ReceiveConfig {
    ReceiveConfig {
        limits: MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap(),
        bindings: MediaBindings::negotiated(10, 9).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy::default(),
    }
}
fn progress(frame: u64, now: u64) -> Progress {
    Progress {
        descriptor: FrameDescriptor {
            frame,
            total_bytes: 3000,
            stride: 1077,
            capture_micros: now,
            reference: frame.checked_sub(1),
        },
        observed_micros: now,
        observation: SourceObservation::Captured,
        pipeline: PipelineState::Running,
    }
}
#[test]
fn negotiated_layout_is_explicit_and_keeps_recovery_distinct() {
    for (v, r) in [(0, 9), (10, 0), (10, 10)] {
        assert_eq!(
            MediaBindings::negotiated(v, r),
            Err(DeliveryError::StaleGeneration)
        );
    }
    assert_eq!(
        MediaBindings::new(10, 9, 10, 10),
        Err(DeliveryError::StaleGeneration)
    );
    let b = config().bindings;
    for c in [Channel::Video, Channel::MediaConfig, Channel::Control] {
        assert_eq!(b.for_channel(c), 10);
    }
    assert_eq!(b.for_channel(Channel::Recovery), 9);
}
#[test]
fn shared_ids_do_not_allow_progress_repair_or_video_to_cross_lanes() {
    let c = config();
    let mut receiver =
        ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    receiver.decoder_configured(0).unwrap();
    let mut sender = SendCache::new(c.limits, c.bindings, c.epoch, SendPolicy::default()).unwrap();
    let mut out = [0; 1150];
    let n = encode_repair(
        0,
        &[RepairRange { start: 0, end: 1 }],
        3,
        10,
        &c.limits,
        &mut out,
    )
    .unwrap();
    assert!(matches!(
        receiver.receive(Channel::MediaConfig, &out[..n], 1),
        Err(DeliveryError::Wire(WireError::WrongChannel))
    ));
    let n = encode_progress(progress(1, 1), 10, &c.limits, &mut out).unwrap();
    assert!(matches!(
        sender.queue_repair(&out[..n], 1),
        Err(SendError::Wire(WireError::WrongChannel))
    ));
    assert!(matches!(
        Record::decode(&out[..n], &c.limits, 10, Channel::Video),
        Err(WireError::WrongChannel)
    ));
}
#[test]
fn negotiated_layout_delivers_idr_and_repairs_loss_reordering_and_missing_final_picture() {
    let c = config();
    let mut receiver =
        ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    receiver.decoder_configured(0).unwrap();
    let mut sender = SendCache::new(c.limits, c.bindings, c.epoch, SendPolicy::default()).unwrap();
    let mut out = [0; 1150];
    let mut repaired = 0;
    for frame in 0_u64..=40 {
        let now = frame * 50000;
        let expected = vec![u8::try_from(frame).unwrap(); 3000];
        sender
            .push(
                progress(frame, now),
                expected.clone(),
                if frame == 0 {
                    DeliveryMode::Recovery
                } else {
                    DeliveryMode::Datagrams
                },
                now,
            )
            .unwrap();
        let mut packets = Vec::new();
        while let Some(p) = sender.next_packet(now, &mut out).unwrap() {
            let bytes = out[..p.byte_len()].to_vec();
            packets.push((p, bytes));
        }
        // Reliable IDR chunks remain ordered; datagrams deliberately do not.
        if frame != 0 {
            packets.reverse();
        }
        for (index, (p, bytes)) in packets.iter().enumerate() {
            if p.channel() == Channel::Video && (frame == 40 || index % 2 == 0) {
                continue;
            }
            receiver.receive(p.channel(), bytes, now).unwrap();
            if p.channel() == Channel::Video {
                receiver.receive(p.channel(), bytes, now).unwrap();
            }
        }
        if frame != 0 {
            assert!(receiver.take_decodable(now).unwrap().is_none());
            let n = receiver
                .repair_request(now + 20000, &mut out)
                .unwrap()
                .unwrap();
            sender.queue_repair(&out[..n], now + 20000).unwrap();
            while let Some(p) = sender.next_repair_packet(now + 30000, &mut out).unwrap() {
                receiver
                    .receive(p.channel(), &out[..p.byte_len()], now + 30000)
                    .unwrap();
                repaired += 1;
            }
        }
        let picture = receiver.take_decodable(now + 31000).unwrap().unwrap();
        assert_eq!(picture.descriptor().frame, frame);
        assert_eq!(picture.bytes(), expected);
        receiver
            .acknowledge_decode(&picture, true, now + 31000)
            .unwrap();
        drop(picture);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
    }
    assert!(repaired > 40);
    sender.tick(3_000_000).unwrap();
    assert_eq!(sender.cached_bytes(), 0);
}
