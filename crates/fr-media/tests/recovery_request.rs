//! Real delivery owners and wire bytes; decoder completion below is simulated,
//! not HEVC, OS presentation or live-transport qualification.
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::delivery::*;
use fr_wire::{
    Channel, FrameDescriptor, MediaLimits, PipelineState, Progress, SourceObservation, WireError,
    decoder::Binding,
    input::{InputDelivery as D, InputDirection as I},
    negotiation::ControlBinding,
    recovery_request::{self, REQUEST_BYTES, Reason, Request},
};
fn config() -> ReceiveConfig {
    ReceiveConfig {
        limits: MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16_384, 64).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy::default(),
    }
}
fn binding() -> Binding {
    Binding {
        parent: ControlBinding {
            id: 10,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        },
        display: 4,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
fn receiver() -> ReceivePipeline {
    let c = config();
    ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap()
}
fn bootstrap(r: &mut ReceivePipeline, q: &mut RecoveryRequestor) -> SendCache {
    let c = config();
    let mut s = SendCache::new(c.limits, c.bindings, c.epoch, SendPolicy::default()).unwrap();
    r.decoder_configured(0).unwrap();
    publish(&mut s, r, 0, 0, true);
    let picture = r.take_decodable(0).unwrap().unwrap();
    let frame = r.complete_decode(&picture, 0).unwrap();
    q.observe_decoded(&frame).unwrap();
    s
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
fn publish(s: &mut SendCache, r: &mut ReceivePipeline, frame: u64, now: u64, deliver: bool) {
    s.push(
        progress(frame, now),
        vec![7; 3000],
        if frame == 0 {
            DeliveryMode::Recovery
        } else {
            DeliveryMode::Datagrams
        },
        now,
    )
    .unwrap();
    let mut out = [0; 1150];
    while let Some(p) = s.next_packet(now, &mut out).unwrap() {
        if deliver || p.channel() == Channel::MediaConfig {
            r.receive(p.channel(), &out[..p.byte_len()], now).unwrap();
        }
    }
}
fn parse(out: &[u8]) -> Request {
    recovery_request::decode(
        out,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        I::ViewerToHost,
        D::Reliable,
    )
    .unwrap()
}
#[test]
fn whole_picture_loss_requests_recovery_with_the_actual_last_decoded_frame() {
    let mut r = receiver();
    let mut q = RecoveryRequestor::new(&r, binding()).unwrap();
    let mut s = bootstrap(&mut r, &mut q);
    publish(&mut s, &mut r, 1, 10, false);
    let mut out = [0; REQUEST_BYTES];
    assert!(q.offer(&mut r, 249_999, &mut out).unwrap().is_none());
    let offer = q.offer(&mut r, 250_010, &mut out).unwrap().unwrap();
    assert_eq!(r.state(), ReceiveState::NeedsRecovery);
    assert_eq!(r.budget_usage(), BudgetUsage::default());
    assert_eq!(
        parse(&out),
        Request {
            reason: Reason::ReferenceExpired,
            last_useful_frame: Some(0)
        }
    );
    assert_eq!(offer.send_by_micros(), 2_250_010);
    q.authorize_write(&r, &offer, 250_010).unwrap();
    q.mark_sent(&r, &offer, 250_010).unwrap();
    assert!(q.offer(&mut r, 300_000, &mut out).unwrap().is_none());
    assert!(q.authorize_write(&r, &offer, 300_000).is_err());
    assert_eq!(q.next_deadline(), Some(2_250_010));
    assert_eq!(
        q.offer(&mut r, 2_250_010, &mut out).unwrap_err(),
        DeliveryError::RecoveryExpired
    );
    assert_eq!(
        q.offer(&mut r, 2_250_011, &mut out).unwrap_err(),
        DeliveryError::WrongState
    );
}
#[test]
fn startup_timeout_and_small_buffers_cannot_renew_the_attempt() {
    let mut r = receiver();
    let mut q = RecoveryRequestor::new(&r, binding()).unwrap();
    r.decoder_configured(0).unwrap();
    assert_eq!(
        q.offer(&mut r, 2_000_000, &mut []).unwrap_err(),
        DeliveryError::Wire(WireError::BufferTooSmall)
    );
    assert_eq!(q.next_deadline(), Some(4_000_000));
    let mut out = [0; REQUEST_BYTES];
    let a = q.offer(&mut r, 2_500_000, &mut out).unwrap().unwrap();
    let original = out;
    assert_eq!(
        parse(&out),
        Request {
            reason: Reason::RecoveryExpired,
            last_useful_frame: None
        }
    );
    let b = q.offer(&mut r, 3_999_999, &mut out).unwrap().unwrap();
    assert_eq!(a.send_by_micros(), b.send_by_micros());
    assert_eq!(out, original);
    assert_eq!(
        q.authorize_write(&r, &a, 4_000_000),
        Err(DeliveryError::RecoveryExpired)
    );
    assert!(RecoveryRequestor::new(&r, binding()).is_err());
}
#[test]
fn decoder_failure_requests_recovery_without_freeing_a_borrowed_picture() {
    let mut r = receiver();
    let mut q = RecoveryRequestor::new(&r, binding()).unwrap();
    let mut s = bootstrap(&mut r, &mut q);
    publish(&mut s, &mut r, 1, 10, true);
    let picture = r.take_decodable(10).unwrap().unwrap();
    let charged = r.budget_usage();
    assert_eq!(
        r.acknowledge_decode(&picture, false, 10),
        Err(DeliveryError::DecodeFailed)
    );
    let mut out = [0; REQUEST_BYTES];
    q.offer(&mut r, 10, &mut out).unwrap().unwrap();
    assert_eq!(
        parse(&out),
        Request {
            reason: Reason::DecodeFailed,
            last_useful_frame: Some(0)
        }
    );
    assert!(!picture.is_live());
    assert_eq!(r.budget_usage(), charged);
    drop(picture);
    assert_eq!(r.budget_usage(), BudgetUsage::default());
}
#[test]
fn a_different_receiver_with_identical_numbers_cannot_supply_decode_evidence() {
    let mut r = receiver();
    let mut q = RecoveryRequestor::new(&r, binding()).unwrap();
    let mut other = receiver();
    let mut oq = RecoveryRequestor::new(&other, binding()).unwrap();
    let mut s = bootstrap(&mut other, &mut oq);
    publish(&mut s, &mut other, 1, 10, true);
    let picture = other.take_decodable(10).unwrap().unwrap();
    let receipt = other.complete_decode(&picture, 10).unwrap();
    assert_eq!(
        q.observe_decoded(&receipt),
        Err(DeliveryError::StaleGeneration)
    );
    r.decoder_configured(0).unwrap();
    let mut out = [0; REQUEST_BYTES];
    q.offer(&mut r, 2_000_000, &mut out).unwrap().unwrap();
    assert_eq!(parse(&out).last_useful_frame, None);
    assert_eq!(
        q.offer(&mut other, 2_000_001, &mut out).unwrap_err(),
        DeliveryError::StaleGeneration
    );
}
#[test]
fn external_replacement_and_close_invalidate_prepared_requests() {
    for close in [false, true] {
        let mut r = receiver();
        let mut q = RecoveryRequestor::new(&r, binding()).unwrap();
        r.decoder_configured(0).unwrap();
        let mut out = [0; REQUEST_BYTES];
        let offer = q.offer(&mut r, 2_000_000, &mut out).unwrap().unwrap();
        if close {
            r.close();
        } else {
            r.replace(
                MediaEpoch {
                    recovery: config().epoch.recovery.next().unwrap(),
                    ..config().epoch
                },
                MediaBindings::new(11, 12, 13, 14).unwrap(),
                2_000_001,
            )
            .unwrap();
        }
        assert_eq!(
            q.authorize_write(&r, &offer, 2_000_001),
            Err(DeliveryError::StaleGeneration)
        );
        assert_eq!(q.next_deadline(), None);
    }
}
#[test]
fn cancellation_malformed_records_and_clock_regression_are_not_recovery_reasons() {
    let mut r = receiver();
    let mut q = RecoveryRequestor::new(&r, binding()).unwrap();
    let mut s = bootstrap(&mut r, &mut q);
    publish(&mut s, &mut r, 1, 10, true);
    let picture = r.take_decodable(10).unwrap().unwrap();
    picture.cancel_decode();
    let mut out = [0; REQUEST_BYTES];
    assert!(q.offer(&mut r, 11, &mut out).is_err());
    assert_eq!(q.next_deadline(), None);
    let mut r = receiver();
    let mut q = RecoveryRequestor::new(&r, binding()).unwrap();
    r.decoder_configured(0).unwrap();
    assert!(r.receive(Channel::Video, b"invalid", 1).is_err());
    assert!(q.offer(&mut r, 2, &mut out).is_err());
    assert_eq!(q.next_deadline(), None);
    let mut r = receiver();
    let mut q = RecoveryRequestor::new(&r, binding()).unwrap();
    r.decoder_configured(0).unwrap();
    q.offer(&mut r, 10, &mut out).unwrap();
    assert_eq!(
        q.offer(&mut r, 9, &mut out).unwrap_err(),
        DeliveryError::ClockRegression
    );
    assert_eq!(
        q.offer(&mut r, 11, &mut out).unwrap_err(),
        DeliveryError::WrongState
    );
}
#[test]
fn a_healthy_peer_is_untouched_by_another_viewers_recovery_request() {
    let mut bad = receiver();
    let mut bq = RecoveryRequestor::new(&bad, binding()).unwrap();
    let mut bs = bootstrap(&mut bad, &mut bq);
    let mut good = receiver();
    let mut gq = RecoveryRequestor::new(&good, binding()).unwrap();
    let mut gs = bootstrap(&mut good, &mut gq);
    publish(&mut bs, &mut bad, 1, 10, false);
    bq.offer(&mut bad, 250_010, &mut [0; REQUEST_BYTES])
        .unwrap()
        .unwrap();
    publish(&mut gs, &mut good, 1, 250_010, true);
    let picture = good.take_decodable(250_010).unwrap().unwrap();
    gq.observe_decoded(&good.complete_decode(&picture, 250_010).unwrap())
        .unwrap();
    assert_eq!(good.state(), ReceiveState::Streaming);
    assert!(
        gq.offer(&mut good, 250_011, &mut [0; REQUEST_BYTES])
            .unwrap()
            .is_none()
    );
}
