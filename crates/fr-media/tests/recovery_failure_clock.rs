//! Receiver-local failure clocks using real wire/reassembly owners and synthetic
//! compressed bytes. These tests do not claim native decode or network evidence.
use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, OsSessionId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    limits::ProtocolLimits,
};
use fr_media::delivery::{
    DeliveryError, MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig, ReceivePipeline,
    ReceivePolicy, ReceiveState, RecoveryRequestor,
};
use fr_wire::{
    Channel, Fragment, FrameDescriptor, MediaLimits, PipelineState, Progress, RecoveryChunk,
    SourceObservation, decoder::Binding, encode_fragment, encode_progress, encode_recovery,
    negotiation::ControlBinding, recovery_request,
};

fn config() -> ReceiveConfig {
    ReceiveConfig {
        limits: MediaLimits::new(ProtocolLimits::ABSOLUTE, 1_150, 16_384, 64).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy::default(),
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
        display: 1,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: c.epoch.configuration,
        recovery: c.epoch.recovery,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
fn bootstrap(r: &mut ReceivePipeline, c: ReceiveConfig, at: u64) {
    r.decoder_configured(at).unwrap();
    let mut out = [0; 1_150];
    let n = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 16,
            offset: 0,
            capture_micros: 0,
            bytes: &[7; 16],
        },
        c.bindings.for_channel(Channel::Recovery),
        &c.limits,
        &mut out,
    )
    .unwrap();
    r.receive(Channel::Recovery, &out[..n], at).unwrap();
    let picture = r.take_decodable(at).unwrap().unwrap();
    r.complete_decode(&picture, at).unwrap();
}
fn ready() -> (ReceiveConfig, ReceivePipeline, RecoveryRequestor) {
    let c = config();
    let mut r = ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    let q = RecoveryRequestor::new(&r, binding(c)).unwrap();
    bootstrap(&mut r, c, 0);
    (c, r, q)
}
fn progress(r: &mut ReceivePipeline, c: ReceiveConfig, failed: bool, at: u64) {
    let mut out = [0; 1_150];
    let n = encode_progress(
        Progress {
            descriptor: FrameDescriptor {
                frame: 1,
                reference: Some(0),
                total_bytes: 16,
                stride: c.limits.fragment_stride(),
                capture_micros: 0,
            },
            observed_micros: 0,
            observation: SourceObservation::Captured,
            pipeline: if failed {
                PipelineState::Failed
            } else {
                PipelineState::Running
            },
        },
        c.bindings.for_channel(Channel::MediaConfig),
        &c.limits,
        &mut out,
    )
    .unwrap();
    let result = r.receive(Channel::MediaConfig, &out[..n], at);
    if failed {
        assert_eq!(result, Err(DeliveryError::ReferenceExpired));
    } else {
        result.unwrap();
    }
}

#[test]
fn failure_time_survives_idle_service_before_first_request_preparation() {
    let (c, mut r, mut q) = ready();
    progress(&mut r, c, true, 100);
    for now in [101, 5_000, 10_000] {
        assert_eq!(r.tick(now), Err(DeliveryError::ReferenceExpired));
    }
    let offer = q
        .offer(&mut r, 20_000, &mut [0; recovery_request::REQUEST_BYTES])
        .unwrap()
        .unwrap();
    // The sender's notice is an abandoned chain, never a local decode failure.
    assert_eq!(offer.reason(), recovery_request::Reason::ReferenceExpired);
    assert_eq!(
        offer.send_by_micros(),
        100 + c.policy.recovery_budget_micros
    );
}

#[test]
fn first_offer_at_or_after_original_deadline_writes_nothing_and_is_terminal() {
    for late in [0, 1] {
        let (c, mut r, mut q) = ready();
        progress(&mut r, c, true, 100);
        let now = 100 + c.policy.recovery_budget_micros + late;
        let mut out = [0xa5; recovery_request::REQUEST_BYTES];
        assert!(matches!(
            q.offer(&mut r, now, &mut out),
            Err(DeliveryError::RecoveryExpired)
        ));
        assert_eq!(out, [0xa5; recovery_request::REQUEST_BYTES]);
        assert_eq!(q.next_deadline(), None);
        assert!(q.offer(&mut r, now + 1, &mut out).is_err());
        assert_eq!(r.state(), ReceiveState::NeedsRecovery);
    }
}

#[test]
fn decode_failure_time_precedes_delayed_requestor_service() {
    let (c, mut r, mut q) = ready();
    let mut out = [0; 1_150];
    let n = encode_fragment(
        Fragment {
            descriptor: FrameDescriptor {
                frame: 1,
                reference: Some(0),
                total_bytes: 16,
                stride: c.limits.fragment_stride(),
                capture_micros: 0,
            },
            index: 0,
            bytes: &[8; 16],
        },
        c.bindings.for_channel(Channel::Video),
        &c.limits,
        &mut out,
    )
    .unwrap();
    r.receive(Channel::Video, &out[..n], 200).unwrap();
    let picture = r.take_decodable(202).unwrap().unwrap();
    assert_eq!(
        r.acknowledge_decode(&picture, false, 203),
        Err(DeliveryError::DecodeFailed)
    );
    let charged = r.budget_usage();
    let offer = q.offer(&mut r, 20_000, &mut out).unwrap().unwrap();
    assert_eq!(
        offer.send_by_micros(),
        203 + c.policy.recovery_budget_micros
    );
    assert_eq!(r.budget_usage(), charged);
    assert_eq!(picture.bytes(), &[8; 16]);
    drop(picture);
    assert_eq!(r.budget_usage().pictures, 0);
}

#[test]
fn reference_expiry_detection_is_not_restarted_by_request_preparation() {
    let (c, mut r, mut q) = ready();
    progress(&mut r, c, false, 500);
    let failed_at = 500 + c.policy.reference_budget_micros;
    assert_eq!(r.tick(failed_at), Err(DeliveryError::ReferenceExpired));
    let offer = q
        .offer(
            &mut r,
            failed_at + 10_000,
            &mut [0; recovery_request::REQUEST_BYTES],
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        offer.send_by_micros(),
        failed_at + c.policy.recovery_budget_micros
    );
}

#[test]
fn new_receiver_generation_does_not_inherit_the_previous_failure_clock() {
    let (c, mut r, mut q) = ready();
    progress(&mut r, c, true, 100);
    let mut out = [0; recovery_request::REQUEST_BYTES];
    let offer = q.offer(&mut r, 110, &mut out).unwrap().unwrap();
    q.mark_sent(&r, &offer, 111).unwrap();
    let next = ReceiveConfig {
        epoch: MediaEpoch {
            recovery: c.epoch.recovery.next().unwrap(),
            ..c.epoch
        },
        bindings: MediaBindings::new(5, 6, 7, 8).unwrap(),
        ..c
    };
    q.authorize_replacement(&r, binding(next), next.limits, next.bindings, 112)
        .unwrap();
    r.replace(next.epoch, next.bindings, 112).unwrap();
    let mut current = RecoveryRequestor::new(&r, binding(next)).unwrap();
    bootstrap(&mut r, next, 112);
    assert!(matches!(
        q.offer(&mut r, 113, &mut out),
        Err(DeliveryError::StaleGeneration)
    ));
    progress(&mut r, next, true, 1_000_000);
    let next_offer = current.offer(&mut r, 1_000_010, &mut out).unwrap().unwrap();
    assert_eq!(
        next_offer.send_by_micros(),
        1_000_000 + next.policy.recovery_budget_micros
    );
}

#[test]
fn detected_failure_deadline_overflow_is_terminal_without_writing() {
    let (c, mut r, mut q) = ready();
    progress(&mut r, c, true, u64::MAX - 100);
    let mut out = [0xa5; recovery_request::REQUEST_BYTES];
    assert!(matches!(
        q.offer(&mut r, u64::MAX - 50, &mut out),
        Err(DeliveryError::ClockOverflow)
    ));
    assert_eq!(out, [0xa5; recovery_request::REQUEST_BYTES]);
    assert_eq!(q.next_deadline(), None);
    assert!(q.offer(&mut r, u64::MAX, &mut out).is_err());
}

#[test]
fn malformed_records_are_not_relabelled_as_expired_recovery() {
    let (_, mut r, mut q) = ready();
    let error = r.receive(Channel::Video, b"malformed", 10).unwrap_err();
    assert!(matches!(error, DeliveryError::Wire(_)));
    let mut out = [0xa5; recovery_request::REQUEST_BYTES];
    assert!(matches!(q.offer(&mut r, 10_000_000, &mut out), Err(cause) if cause == error));
    assert_eq!(out, [0xa5; recovery_request::REQUEST_BYTES]);
    assert_eq!(q.next_deadline(), None);
}

#[test]
fn undersized_output_and_backpressure_keep_the_receivers_original_failure_time() {
    let (c, mut r, mut q) = ready();
    progress(&mut r, c, true, 100);
    assert!(matches!(
        q.offer(&mut r, 110, &mut []),
        Err(DeliveryError::Wire(_))
    ));
    let until = 100 + c.policy.recovery_budget_micros;
    assert_eq!(q.next_deadline(), Some(until));
    let mut out = [0; recovery_request::REQUEST_BYTES];
    let offer = q.offer(&mut r, 500, &mut out).unwrap().unwrap();
    assert_eq!(offer.send_by_micros(), until);
    let original = out;
    let again = q.offer(&mut r, 501, &mut out).unwrap().unwrap();
    assert_eq!(again.send_by_micros(), until);
    assert_eq!(out, original);
    q.mark_sent(&r, &offer, 502).unwrap();
    assert!(q.offer(&mut r, 503, &mut out).unwrap().is_none());
    assert_eq!(q.next_deadline(), Some(until));
}
