//! Production packetizers, parsers and recovery owners. Decoder completion is
//! explicitly simulated; these tests do not qualify HEVC or live transport.
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
fn view() -> Binding {
    Binding {
        parent: ControlBinding {
            id: 10,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        },
        display: 4,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: config().epoch.configuration,
        recovery: config().epoch.recovery,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
fn sender() -> SendCache {
    let c = config();
    SendCache::new(c.limits, c.bindings, c.epoch, SendPolicy::default()).unwrap()
}
fn receiver() -> ReceivePipeline {
    let c = config();
    ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap()
}
fn progress(frame: u64, reference: Option<u64>, now: u64) -> Progress {
    Progress {
        descriptor: FrameDescriptor {
            frame,
            total_bytes: 3000,
            stride: 1077,
            capture_micros: now,
            reference,
        },
        observed_micros: now,
        observation: SourceObservation::Captured,
        pipeline: PipelineState::Running,
    }
}
fn push(s: &mut SendCache, frame: u64, reference: Option<u64>, now: u64) {
    s.push(
        progress(frame, reference, now),
        vec![7; 3000],
        if reference.is_none() {
            DeliveryMode::Recovery
        } else {
            DeliveryMode::Datagrams
        },
        now,
    )
    .unwrap();
}
fn deliver(s: &mut SendCache, r: &mut ReceivePipeline, now: u64, drop_video: bool) {
    let mut out = [0; 1150];
    while let Some(p) = s.next_packet(now, &mut out).unwrap() {
        s.authorize_write(&p, now).unwrap();
        if !drop_video || p.channel() != Channel::Video {
            r.receive(p.channel(), &out[..p.byte_len()], now).unwrap();
        }
    }
}
fn request(binding: Binding, last: Option<u64>) -> [u8; REQUEST_BYTES] {
    let mut out = [0; REQUEST_BYTES];
    recovery_request::encode(
        Request {
            reason: Reason::ReferenceExpired,
            last_useful_frame: last,
        },
        binding,
        &ProtocolLimits::ABSOLUTE,
        &mut out,
        I::ViewerToHost,
        D::Reliable,
    )
    .unwrap();
    out
}
fn accepted(s: &mut SendCache, binding: Binding, now: u64) -> RecoveryDemand {
    match s
        .request_recovery(&request(binding, None), binding, now)
        .unwrap()
    {
        RecoveryDisposition::Accepted(demand) => demand,
        RecoveryDisposition::Coalesced => panic!("expected one new demand"),
    }
}
fn replacement() -> (MediaEpoch, MediaBindings, Binding) {
    let epoch = MediaEpoch {
        recovery: config().epoch.recovery.next().unwrap(),
        ..config().epoch
    };
    (
        epoch,
        MediaBindings::new(11, 12, 13, 14).unwrap(),
        Binding {
            recovery: epoch.recovery,
            ..view()
        },
    )
}
#[test]
fn loss_to_request_to_fresh_reliable_chain_preserves_a_healthy_viewer() {
    let mut bad = receiver();
    let mut bq = RecoveryRequestor::new(&bad, view()).unwrap();
    let mut bs = sender();
    let mut good = receiver();
    let mut gs = sender();
    bad.decoder_configured(0).unwrap();
    good.decoder_configured(0).unwrap();
    for (index, (s, r)) in [(&mut bs, &mut bad), (&mut gs, &mut good)]
        .into_iter()
        .enumerate()
    {
        push(s, 0, None, 0);
        deliver(s, r, 0, false);
        let picture = r.take_decodable(0).unwrap().unwrap();
        let receipt = r.complete_decode(&picture, 0).unwrap();
        if index == 0 {
            bq.observe_decoded(&receipt).unwrap();
        }
    }
    push(&mut bs, 1, Some(0), 10);
    deliver(&mut bs, &mut bad, 10, true);
    let mut request = [0; REQUEST_BYTES];
    let offer = bq.offer(&mut bad, 250_010, &mut request).unwrap().unwrap();
    bq.authorize_write(&bad, &offer, 250_010).unwrap();
    let demand = match bs.request_recovery(&request, view(), 250_010).unwrap() {
        RecoveryDisposition::Accepted(demand) => demand,
        RecoveryDisposition::Coalesced => panic!("not sent before"),
    };
    assert_eq!(demand.request().last_useful_frame, Some(0));
    bq.mark_sent(&bad, &offer, 250_010).unwrap();
    assert!(bs.needs_recovery());
    assert_eq!(bs.cached_bytes(), 0);
    assert_eq!(
        bs.next_packet(250_010, &mut [0; 1150]),
        Err(SendError::NeedsRecovery)
    );
    let mut idr = IdrCoalescer::new(500_000).unwrap();
    idr.queue(&bs, demand, 250_010).unwrap();
    assert_eq!(idr.take(250_010).unwrap(), Some(2_250_010));
    assert_eq!(idr.take(250_010).unwrap(), None);
    // This models successful session-owner admission of NEW bindings, not an
    // implicit permission grant. Existing decoder completion still gates deltas.
    let (epoch, bindings, binding) = replacement();
    bs.replace(epoch, bindings, 250_020).unwrap();
    bad.replace(epoch, bindings, 250_020).unwrap();
    bad.decoder_configured(250_020).unwrap();
    assert!(bq.authorize_write(&bad, &offer, 250_020).is_err());
    assert!(bs.request_recovery(&request, binding, 250_020).is_err());
    push(&mut bs, 5, None, 250_020);
    deliver(&mut bs, &mut bad, 250_020, false);
    let recovered = bad.take_decodable(250_020).unwrap().unwrap();
    let receipt = bad.complete_decode(&recovered, 250_020).unwrap();
    assert_eq!(receipt.descriptor().frame, 5);
    assert_eq!(bad.state(), ReceiveState::Streaming);
    assert!(recovered.is_live());
    push(&mut bs, 6, Some(5), 250_030);
    deliver(&mut bs, &mut bad, 250_030, false);
    let delta = bad.take_decodable(250_030).unwrap().unwrap();
    assert_eq!(
        bad.complete_decode(&delta, 250_030)
            .unwrap()
            .descriptor()
            .frame,
        6
    );
    // The same old generation and decoder on the unaffected viewer continue.
    push(&mut gs, 1, Some(0), 250_030);
    deliver(&mut gs, &mut good, 250_030, false);
    let picture = good.take_decodable(250_030).unwrap().unwrap();
    assert_eq!(
        good.complete_decode(&picture, 250_030).unwrap().epoch(),
        config().epoch
    );
    assert_eq!(good.state(), ReceiveState::Streaming);
}
#[test]
fn accepted_requests_fence_prepared_packets_and_coalesce_without_deadline_renewal() {
    let mut s = sender();
    push(&mut s, 0, None, 0);
    let packet = s.next_packet(0, &mut [0; 1150]).unwrap().unwrap();
    let demand = accepted(&mut s, view(), 10);
    assert_eq!(demand.deadline_micros(), 2_000_010);
    assert_eq!(
        s.authorize_write(&packet, 10),
        Err(SendError::NeedsRecovery)
    );
    assert_eq!(s.cached_pictures(), 0);
    assert!(!s.can_push_capacity(1));
    assert!(!s.originals_pending());
    let bytes = request(view(), Some(0));
    assert!(matches!(
        s.request_recovery(&bytes, view(), 1_000_000).unwrap(),
        RecoveryDisposition::Coalesced
    ));
    assert_eq!(s.next_deadline(), Some(2_000_010));
    let (epoch, bindings, _) = replacement();
    assert_eq!(
        s.replace(epoch, bindings, 2_000_010),
        Err(SendError::Delivery(DeliveryError::RecoveryExpired))
    );
    assert_eq!(s.next_deadline(), None);
    assert_eq!(s.tick(2_000_011), Err(SendError::Closed));
    assert!(s.request_recovery(&bytes, view(), 2_000_011).is_err());
}
#[test]
fn invalid_requests_leave_original_packets_and_cache_usable() {
    let mut s = sender();
    push(&mut s, 0, None, 0);
    let packet = s.next_packet(0, &mut [0; 1150]).unwrap().unwrap();
    let charged = s.cached_bytes();
    assert_eq!(
        s.request_recovery(&request(view(), Some(1)), view(), 0)
            .unwrap_err(),
        SendError::InvalidSequence
    );
    let foreign = Binding {
        display: 5,
        ..view()
    };
    assert_eq!(
        s.request_recovery(&request(foreign, None), view(), 0)
            .unwrap_err(),
        SendError::Wire(WireError::InvalidBinding)
    );
    assert!(s.request_recovery(b"bad", view(), 0).is_err());
    let future = Binding {
        recovery: view().recovery.next().unwrap(),
        ..view()
    };
    assert!(
        s.request_recovery(&request(future, None), future, 0)
            .is_err()
    );
    assert_eq!(s.cached_bytes(), charged);
    assert!(!s.needs_recovery());
    s.authorize_write(&packet, 0).unwrap();
    assert!(s.next_packet(0, &mut [0; 1150]).unwrap().is_some());
}
#[test]
fn fresh_generation_rejects_old_packets_and_demands_even_with_reused_frame_numbers() {
    let mut s = sender();
    push(&mut s, 0, None, 0);
    let packet = s.next_packet(0, &mut [0; 1150]).unwrap().unwrap();
    let demand = accepted(&mut s, view(), 10);
    let (epoch, bindings, _) = replacement();
    s.replace(epoch, bindings, 20).unwrap();
    push(&mut s, 0, None, 20);
    assert_eq!(
        s.authorize_write(&packet, 20),
        Err(SendError::Delivery(DeliveryError::StaleGeneration))
    );
    assert_eq!(
        IdrCoalescer::new(500_000).unwrap().queue(&s, demand, 20),
        Err(DeliveryError::StaleGeneration)
    );
    assert!(
        s.request_recovery(&request(view(), None), view(), 20)
            .is_err()
    );
    assert!(!s.needs_recovery());
}
#[test]
fn shared_idr_work_is_coalesced_and_generations_do_not_refill_rate_credit() {
    let mut a = sender();
    let mut b = sender();
    let ad = accepted(&mut a, view(), 100);
    let bd = accepted(&mut b, view(), 200);
    let mut coalescer = IdrCoalescer::new(500_000).unwrap();
    coalescer.queue(&a, ad, 200).unwrap();
    coalescer.queue(&b, bd, 200).unwrap();
    assert_eq!(coalescer.next_deadline(), Some(200));
    assert_eq!(coalescer.take(200).unwrap(), Some(2_000_100));
    assert_eq!(coalescer.take(200).unwrap(), None);
    let (epoch, bindings, binding) = replacement();
    a.replace(epoch, bindings, 300).unwrap();
    let again = accepted(&mut a, binding, 300);
    coalescer.queue(&a, again, 300).unwrap();
    assert_eq!(coalescer.next_deadline(), Some(500_200));
    assert_eq!(coalescer.take(500_199).unwrap(), None);
    assert_eq!(coalescer.take(500_200).unwrap(), Some(2_000_300));
}
#[test]
fn expired_foreign_or_closed_demands_cannot_schedule_native_work() {
    let mut a = sender();
    let b = sender();
    let demand = accepted(&mut a, view(), 0);
    let mut coalescer = IdrCoalescer::new(100_000).unwrap();
    assert_eq!(
        coalescer.queue(&b, demand, 0),
        Err(DeliveryError::StaleGeneration)
    );
    assert_eq!(coalescer.take(0).unwrap(), None);
    let mut a = sender();
    let demand = accepted(&mut a, view(), 0);
    assert_eq!(
        coalescer.queue(&a, demand, 2_000_000),
        Err(DeliveryError::RecoveryExpired)
    );
    assert_eq!(coalescer.next_deadline(), None);
    let mut a = sender();
    let demand = accepted(&mut a, view(), 2_000_001);
    a.close();
    assert_eq!(
        coalescer.queue(&a, demand, 2_000_001),
        Err(DeliveryError::StaleGeneration)
    );
}
#[test]
fn cancellation_keeps_rate_credit_and_waiting_cohorts_expire() {
    let mut a = sender();
    let mut coalescer = IdrCoalescer::new(1_000_000).unwrap();
    let demand = accepted(&mut a, view(), 0);
    coalescer.queue(&a, demand, 0).unwrap();
    coalescer.take(0).unwrap().unwrap();
    let c = config();
    let mut short = SendCache::new(
        c.limits,
        c.bindings,
        c.epoch,
        SendPolicy {
            recovery_horizon_micros: 200_000,
            ..SendPolicy::default()
        },
    )
    .unwrap();
    let demand = accepted(&mut short, view(), 1);
    coalescer.queue(&short, demand, 1).unwrap();
    assert_eq!(coalescer.next_deadline(), Some(200_001));
    assert_eq!(coalescer.take(200_001), Err(DeliveryError::RecoveryExpired));
    assert_eq!(coalescer.next_deadline(), None);
    let mut b = sender();
    let demand = accepted(&mut b, view(), 200_002);
    coalescer.queue(&b, demand, 200_002).unwrap();
    coalescer.cancel_pending();
    let mut d = sender();
    let demand = accepted(&mut d, view(), 200_003);
    coalescer.queue(&d, demand, 200_003).unwrap();
    assert_eq!(coalescer.take(999_999).unwrap(), None);
    assert_eq!(coalescer.take(1_000_000).unwrap(), Some(2_200_003));
}
#[test]
fn local_clock_failure_and_request_deadline_overflow_are_terminal() {
    assert_eq!(
        IdrCoalescer::new(99_999).unwrap_err(),
        DeliveryError::InvalidPolicy
    );
    assert_eq!(
        IdrCoalescer::new(1_000_001).unwrap_err(),
        DeliveryError::InvalidPolicy
    );
    let mut c = IdrCoalescer::new(100_000).unwrap();
    c.take(10).unwrap();
    assert_eq!(c.take(9), Err(DeliveryError::ClockRegression));
    assert_eq!(c.take(10), Err(DeliveryError::WrongState));
    let mut s = sender();
    assert_eq!(
        s.request_recovery(&request(view(), None), view(), u64::MAX)
            .unwrap_err(),
        SendError::Delivery(DeliveryError::ClockOverflow)
    );
    assert_eq!(s.tick(u64::MAX), Err(SendError::Closed));
}
