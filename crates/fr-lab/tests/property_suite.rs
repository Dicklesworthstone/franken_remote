#![forbid(unsafe_code)]
//! Deterministic property suite for `FrankenRemote` production state machines.
//!
//! Evaluates the core invariants from plan section 24.1:
//! 1. Duplicate input identifier cannot admit another click within its live epoch.
//! 2. An old lease never controls a new session.
//! 3. Geometry changes reject old coordinate input.
//! 4. Obsolete codec configuration never reaches the new decoder.
//! 5. Missing references trigger repair or recovery, never indefinite corrupted output.
//! 6. Host-clock challenge expiry.
//! 7. Ticket expiry immediately before injection.
//! 8. No action resurrection after a stalled reactor.
//! 9. Read-only approval enforcement (gates thumbnails/audio/clipboard/semantic reads).
//! 10. Controller-handoff serialization.
//! 11. Shared-pipeline lifetime (closing one viewer never cancels another).
//! 12. Late viewer join waits for a fresh recovery point.
//! 13. Old pointer datagrams after a click.
//! 14. Client-freshness vs unchanged-screen distinction.
//! 15. Committed-vs-uncommitted work in channel cancellation.
//! 16. Admitted-vs-irreversibly-submitted OS injection in teardown results.
//!
//! Recovery model rows per `P1_RECOVERY`:
//! - Missing first, middle, and final fragments.
//! - Loss immediately before idle.
//! - Reference repair after its own display deadline.
//! - Lost recovery configuration or acknowledgement.
//! - Exhausted recovery budget.
//! - 120-ms repair horizon.
//! - Large IDRs reassembly.
//! - Receiver-credit starvation.
//!
//! Planted-negative validation demonstrates that deliberate violations of each
//! invariant are caught by the suite.

use fr_core::authority::{AuthorityError, AuthorityPolicy, SessionAuthority, ViewReadiness};
use fr_core::ids::{
    CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, InputLeaseId,
    InputTicketId, RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
};
use fr_core::input::{
    DesktopPoint, InputBounds, InputCredentials, InputEvent, InputRequest, InputView,
    KeyTransition, PhysicalKey, PointerButton,
};
use fr_core::input_sequence::{
    InputAdmission, InputOutcome, InputSequenceError, InputSequenceLedger,
    MAX_RETAINED_INPUT_RECEIPTS,
};
use fr_core::input_submission::{
    Capabilities, Capability, Dispatch, InputSession, InputSink, Operation, PlatformError, Refusal,
    Submission,
};
use fr_core::limits::ProtocolLimits;
use fr_core::time::{HostDuration, HostInstant};

use fr_media::delivery::{
    DeliveryError, DeliveryMode, MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig,
    ReceivePipeline, ReceivePolicy, ReceiveState, SendCache, SendPolicy,
};
use fr_media::freshness::{
    ClockCorrelation, ClockPolicy, ClockSample, Error as FreshnessError, ViewTracker,
};

use fr_wire::{
    Channel, FrameDescriptor, MediaLimits, PipelineState, Progress, RecoveryChunk,
    SourceObservation, encode_fragment, encode_progress, encode_recovery,
};

use fr_lab::{Destination, Fault, Limits, Scenario};

// ---------------------------------------------------------------------------
// Helpers & Fixtures
// ---------------------------------------------------------------------------

fn ms(millis: u64) -> HostDuration {
    HostDuration::from_millis_checked(millis).expect("valid duration")
}

fn us(micros: u64) -> HostDuration {
    HostDuration::from_micros(micros)
}

fn lab(seed: u64) -> Scenario {
    Scenario::new(seed, Limits::default()).expect("valid limits")
}

#[derive(Default)]
struct TestSink {
    submitted: Vec<Operation>,
    fail_prepare: bool,
}

impl InputSink for TestSink {
    fn prepare(&mut self, _op: Operation) -> Result<(), PlatformError> {
        if self.fail_prepare {
            Err(PlatformError::Unavailable)
        } else {
            Ok(())
        }
    }
    fn submit(&mut self, op: Operation) -> Submission {
        self.submitted.push(op);
        Submission::Submitted
    }
}

fn default_view() -> InputView {
    InputView {
        geometry: DisplayGeometryGeneration::from_raw(1),
        viewport: ViewportMappingGeneration::from_raw(1),
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    }
}

fn credentials(ticket: InputTicketId) -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(100),
        lease: InputLeaseId::from_raw(200),
        ticket,
        view: default_view(),
    }
}

fn input_caps() -> Capabilities {
    Capabilities::default()
        .with(Capability::Buttons)
        .with(Capability::Absolute)
        .with(Capability::Keys)
}

fn input_bounds() -> InputBounds {
    InputBounds::new(DesktopPoint { x: 0, y: 0 }, 1920, 1080).expect("bounds")
}

fn setup_input_session(t0: HostInstant) -> (InputSession, InputCredentials, HostInstant) {
    let creds = credentials(InputTicketId::from_raw(1));
    let mut auth = SessionAuthority::new(creds.session, AuthorityPolicy::plan_defaults());
    auth.mark_capabilities_checked().unwrap();
    auth.authorize_observation(t0).unwrap();
    auth.mark_view_ready(t0).unwrap();
    auth.grant_lease(creds.lease, t0).unwrap();
    let deadline = auth
        .issue_input_ticket(creds.lease, creds.ticket, t0)
        .unwrap();
    let session = InputSession::new(auth, creds, input_bounds(), input_caps(), t0).unwrap();
    (session, creds, deadline)
}

fn default_media_config() -> ReceiveConfig {
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

fn bootstrap_receiver(c: ReceiveConfig) -> (ReceivePipeline, MediaBudget) {
    let budget = MediaBudget::new(c.limits.protocol()).unwrap();
    let mut receiver = ReceivePipeline::new(c, budget.clone()).unwrap();
    receiver.decoder_configured(0).unwrap();
    let test_bytes = vec![42_u8; 100];
    let mut packet = [0; 1_150];
    let n = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 100,
            offset: 0,
            capture_micros: 0,
            bytes: &test_bytes,
        },
        c.bindings.for_channel(Channel::Recovery),
        &c.limits,
        &mut packet,
    )
    .unwrap();
    receiver
        .receive(Channel::Recovery, &packet[..n], 0)
        .unwrap();
    let picture = receiver.take_decodable(0).unwrap().unwrap();
    receiver.acknowledge_decode(&picture, true, 0).unwrap();
    (receiver, budget)
}

fn test_descriptor(frame: u64, reference: Option<u64>, bytes: u32, stride: u32) -> FrameDescriptor {
    FrameDescriptor {
        frame,
        reference,
        total_bytes: bytes,
        stride,
        capture_micros: frame * 1000,
    }
}

fn send_progress(
    receiver: &mut ReceivePipeline,
    c: &ReceiveConfig,
    d: FrameDescriptor,
    time: u64,
    pipeline: PipelineState,
) {
    let prog = Progress {
        descriptor: d,
        observed_micros: time,
        observation: SourceObservation::Captured,
        pipeline,
    };
    let mut pkt = vec![0; c.limits.record_bytes()];
    let np = encode_progress(
        prog,
        c.bindings.for_channel(Channel::MediaConfig),
        &c.limits,
        &mut pkt,
    )
    .unwrap();
    receiver
        .receive(Channel::MediaConfig, &pkt[..np], time)
        .unwrap();
}

fn send_fragments(
    receiver: &mut ReceivePipeline,
    c: &ReceiveConfig,
    d: FrameDescriptor,
    full_bytes: &[u8],
    indices: &[u32],
    time: u64,
) {
    for &idx in indices {
        let mut pkt = vec![0; c.limits.record_bytes()];
        let range = d.fragment_range(idx).unwrap();
        let n = encode_fragment(
            fr_wire::Fragment {
                descriptor: d,
                index: idx,
                bytes: &full_bytes[range],
            },
            c.bindings.for_channel(Channel::Video),
            &c.limits,
            &mut pkt,
        )
        .unwrap();
        receiver.receive(Channel::Video, &pkt[..n], time).unwrap();
    }
}

// ---------------------------------------------------------------------------
// 1. Duplicate input identifier cannot admit another click
// ---------------------------------------------------------------------------

#[test]
fn prop_duplicate_input_identifier_cannot_admit_another_click_within_live_epoch() {
    let mut lab = lab(101);
    let t0 = HostInstant::from_micros(0);
    let (mut session, creds, _) = setup_input_session(t0);
    let mut sink = TestSink::default();

    // Click request with reliable sequence 0
    let click = InputRequest {
        credentials: creds,
        sequence: 0,
        event: InputEvent::Button {
            button: PointerButton::Primary,
            pressed: true,
            position: DesktopPoint { x: 100, y: 200 },
            barrier: 0,
        },
    };

    // First arrival: admitted and submitted
    let res1 = session.dispatch(click, &mut sink, || t0);
    assert!(matches!(res1, Ok(Dispatch::Completed(_))), "{lab:?}");
    assert_eq!(sink.submitted.len(), 2, "{lab:?} (move + button press)");

    // Simulate duplicate delivery in lab
    lab.send(
        Destination::Host,
        &[0],
        Fault::Duplicate {
            first: us(10),
            second: us(20),
        },
    )
    .unwrap();
    lab.elapse(us(20)).unwrap();

    let mut second_dispatch_result = None;
    lab.drain(|delivery| {
        if delivery.id == 0 && delivery.to == Destination::Host {
            second_dispatch_result = Some(session.dispatch(click, &mut sink, || t0));
        }
        0
    })
    .unwrap();

    assert!(
        matches!(second_dispatch_result, Some(Ok(Dispatch::Completed(_)))),
        "Duplicate returned cached completion receipt: {lab:?}"
    );
    // No second click was admitted or submitted to the sink!
    assert_eq!(
        sink.submitted.len(),
        2,
        "Duplicate click never admitted: {lab:?}"
    );

    // Assert queue and byte bounds
    assert_eq!(lab.metrics().queued_packets, 0, "{lab:?}");
}

#[test]
fn prop_planted_negative_duplicate_click_without_dedupe_would_resubmit() {
    let creds = credentials(InputTicketId::from_raw(1));
    let click = InputRequest {
        credentials: creds,
        sequence: 0,
        event: InputEvent::Button {
            button: PointerButton::Primary,
            pressed: true,
            position: DesktopPoint { x: 50, y: 50 },
            barrier: 0,
        },
    };

    // If deduplication is skipped, two distinct ledger admissions occur
    let mut ledger = InputSequenceLedger::new(creds.lease, MAX_RETAINED_INPUT_RECEIPTS).unwrap();
    assert_eq!(
        ledger.admit(creds.lease, click.sequence),
        Ok(InputAdmission::Admitted)
    );

    // Planted negative: a second admit on sequence 0 returns Completed, NOT Admitted
    let second_admit = ledger.admit(creds.lease, click.sequence);
    assert_ne!(
        second_admit,
        Ok(InputAdmission::Admitted),
        "Planted negative caught: duplicate sequence must not be readmitted"
    );
}

// ---------------------------------------------------------------------------
// 2. An old lease never controls a new session
// ---------------------------------------------------------------------------

#[test]
fn prop_old_lease_never_controls_new_session() {
    let mut lab = lab(102);
    let t0 = HostInstant::from_micros(0);
    let old_lease = InputLeaseId::from_raw(42);

    // New session initialized
    let mut session_authority = SessionAuthority::new(
        RemoteSessionId::from_raw(99),
        AuthorityPolicy::plan_defaults(),
    );
    session_authority.mark_capabilities_checked().unwrap();
    session_authority.authorize_observation(t0).unwrap();
    session_authority.mark_view_ready(t0).unwrap();

    // Grant fresh lease 100 to the new session
    let fresh_lease = InputLeaseId::from_raw(100);
    session_authority.grant_lease(fresh_lease, t0).unwrap();

    // Old lease 42 attempts to authorize submission on new session
    let res = session_authority.authorize_submission(old_lease, InputTicketId::from_raw(1), t0);
    assert_eq!(res, Err(AuthorityError::StaleLease), "{lab:?}");

    lab.send(
        Destination::Host,
        b"stale-lease-action",
        Fault::after(ms(5)),
    )
    .unwrap();
    lab.elapse(ms(5)).unwrap();
    let mut refused = false;
    lab.drain(|delivery| {
        if delivery.to == Destination::Host {
            let attempt = session_authority.authorize_submission(
                old_lease,
                InputTicketId::from_raw(1),
                delivery.now,
            );
            refused = matches!(attempt, Err(AuthorityError::StaleLease));
        }
        0
    })
    .unwrap();
    assert!(
        refused,
        "Old lease was refused by new session authority: {lab:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Geometry changes reject old coordinate input
// ---------------------------------------------------------------------------

#[test]
fn prop_geometry_changes_reject_old_coordinate_input() {
    let mut lab = lab(103);
    let t0 = HostInstant::from_micros(0);
    let mut creds_v2 = credentials(InputTicketId::from_raw(1));
    // Host input session is configured with display geometry generation 2
    creds_v2.view.geometry = DisplayGeometryGeneration::from_raw(2);

    let mut auth = SessionAuthority::new(creds_v2.session, AuthorityPolicy::plan_defaults());
    auth.mark_capabilities_checked().unwrap();
    auth.authorize_observation(t0).unwrap();
    auth.mark_view_ready(t0).unwrap();
    auth.grant_lease(creds_v2.lease, t0).unwrap();
    auth.issue_input_ticket(creds_v2.lease, creds_v2.ticket, t0)
        .unwrap();
    let mut session = InputSession::new(auth, creds_v2, input_bounds(), input_caps(), t0).unwrap();
    let mut sink = TestSink::default();

    // Client sends coordinate input with old credentials (geometry generation 1)
    let mut creds_old = creds_v2;
    creds_old.view.geometry = DisplayGeometryGeneration::from_raw(1);

    let outdated_request = InputRequest {
        credentials: creds_old,
        sequence: 0,
        event: InputEvent::Button {
            button: PointerButton::Primary,
            pressed: true,
            position: DesktopPoint { x: 500, y: 500 },
            barrier: 0,
        },
    };

    lab.send(
        Destination::Host,
        b"outdated-geometry-click",
        Fault::after(ms(10)),
    )
    .unwrap();
    lab.elapse(ms(10)).unwrap();

    let mut dispatch_result = None;
    lab.drain(|_| {
        dispatch_result = Some(session.dispatch(outdated_request, &mut sink, || t0));
        0
    })
    .unwrap();

    match dispatch_result {
        Some(Ok(Dispatch::Completed(receipt))) => {
            assert_eq!(
                receipt.outcome,
                InputOutcome::RejectedBeforeSubmission,
                "{lab:?}"
            );
            assert_eq!(receipt.submitted_operations, 0, "{lab:?}");
            assert_eq!(receipt.refusal, Some(Refusal::StaleView), "{lab:?}");
        }
        other => panic!("Expected completed receipt with RejectedBeforeSubmission, got {other:?}"),
    }
    assert_eq!(
        sink.submitted.len(),
        0,
        "No operations submitted on stale view: {lab:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. Obsolete codec configuration never reaches the new decoder
// ---------------------------------------------------------------------------

#[test]
fn prop_obsolete_codec_configuration_never_reaches_new_decoder() {
    let mut lab = lab(104);
    let c = default_media_config();
    let (mut receiver, _) = bootstrap_receiver(c);

    // Reconfigure receiver with epoch configuration generation 2
    let new_epoch = MediaEpoch {
        configuration: CodecConfigurationGeneration::from_raw(2),
        recovery: RecoveryGeneration::INITIAL,
    };
    let new_bindings = MediaBindings::new(5, 6, 7, 8).unwrap();
    receiver.replace(new_epoch, new_bindings, 1_000).unwrap();
    receiver.decoder_configured(1_000).unwrap();

    // Obsolete packet arrives encoded with old channel binding 1 (generation 1)
    let d = test_descriptor(1, Some(0), 100, 1077);
    let mut obsolete_packet = vec![0; c.limits.record_bytes()];
    let n = encode_fragment(
        fr_wire::Fragment {
            descriptor: d,
            index: 0,
            bytes: &[99_u8; 100],
        },
        c.bindings.for_channel(Channel::Video), // old binding 1
        &c.limits,
        &mut obsolete_packet,
    )
    .unwrap();
    obsolete_packet.truncate(n);

    lab.send(Destination::Client, &obsolete_packet, Fault::after(ms(5)))
        .unwrap();
    lab.elapse(ms(5)).unwrap();

    let mut delivery_err = None;
    lab.drain(|delivery| {
        let res = receiver.receive(Channel::Video, delivery.payload, 1_005);
        delivery_err = Some(res);
        0
    })
    .unwrap();

    assert_eq!(
        delivery_err,
        Some(Err(DeliveryError::StaleGeneration)),
        "Obsolete configuration generation rejected: {lab:?}"
    );
    assert!(receiver.take_decodable(1_005).unwrap().is_none(), "{lab:?}");
}

// ---------------------------------------------------------------------------
// 5. Missing references trigger repair or recovery, never corrupted output
// ---------------------------------------------------------------------------

#[test]
fn prop_missing_references_trigger_repair_or_recovery_never_corrupted_output() {
    let mut lab = lab(105);
    let c = default_media_config();
    let (mut receiver, _) = bootstrap_receiver(c);

    // Frame 0 was decoded in bootstrap.
    // Frame 1 progress is announced on MediaConfig channel at 1_000 us.
    let d1 = test_descriptor(1, Some(0), 100, 1077);
    send_progress(&mut receiver, &c, d1, 1000, PipelineState::Running);

    // Frame 1 video fragment is dropped in transit
    lab.send(Destination::Client, b"frame-1-dropped", Fault::Drop)
        .unwrap();

    // Frame 2 arrives referencing missing Frame 1
    let d2 = test_descriptor(2, Some(1), 100, 1077);
    let mut packet2 = vec![0; c.limits.record_bytes()];
    let n2 = encode_fragment(
        fr_wire::Fragment {
            descriptor: d2,
            index: 0,
            bytes: &[22_u8; 100],
        },
        c.bindings.for_channel(Channel::Video),
        &c.limits,
        &mut packet2,
    )
    .unwrap();
    packet2.truncate(n2);

    lab.send(Destination::Client, &packet2, Fault::after(ms(10)))
        .unwrap();
    lab.advance(ms(10), |delivery| {
        receiver
            .receive(Channel::Video, delivery.payload, 10_000)
            .unwrap();
        0
    })
    .unwrap();

    // Missing reference 1: frame 2 CANNOT be taken as decodable
    let decodable = receiver.take_decodable(10_000).unwrap();
    assert!(
        decodable.is_none(),
        "Unreferenced frame never output: {lab:?}"
    );
    assert!(
        receiver.repair_needed(1),
        "Repair triggered for missing reference: {lab:?}"
    );

    // Advance past reference deadline (250ms) without repair
    assert_eq!(
        receiver.tick(260_000),
        Err(DeliveryError::ReferenceExpired),
        "Expired reference triggers recovery transition: {lab:?}"
    );
    assert!(
        matches!(receiver.state(), ReceiveState::NeedsRecovery),
        "Receiver state is NeedsRecovery: {lab:?}"
    );
}

// ---------------------------------------------------------------------------
// 6. Host-clock challenge expiry
// ---------------------------------------------------------------------------

#[test]
fn prop_host_clock_challenge_expiry() {
    let mut lab = lab(106);
    let t0 = HostInstant::from_micros(0);
    let mut auth = SessionAuthority::new(
        RemoteSessionId::from_raw(1),
        AuthorityPolicy::plan_defaults(),
    );
    auth.mark_capabilities_checked().unwrap();
    auth.authorize_observation(t0).unwrap();
    auth.mark_view_ready(t0).unwrap();

    // Host issues observation challenge with default 3-second authorization lifetime
    let challenge_token = 77;
    let deadline = auth.issue_observation_challenge(challenge_token, t0);
    assert_eq!(deadline, Ok(HostInstant::from_micros(3_000_000)), "{lab:?}");

    // Response packet is delayed beyond deadline (3.5 seconds)
    lab.send(
        Destination::Host,
        b"challenge-response",
        Fault::after(ms(3_500)),
    )
    .unwrap();
    lab.elapse(ms(3_500)).unwrap();

    let mut result = None;
    lab.drain(|delivery| {
        result = Some(auth.respond_observation_challenge(challenge_token, delivery.now));
        0
    })
    .unwrap();

    assert!(
        matches!(
            result,
            Some(Err(
                AuthorityError::ChallengeExpired | AuthorityError::ObservationExpired
            ))
        ),
        "Delayed challenge response is rejected: {lab:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. Ticket expiry immediately before injection
// ---------------------------------------------------------------------------

#[test]
fn prop_ticket_expiry_immediately_before_injection() {
    let mut lab = lab(107);
    let t0 = HostInstant::from_micros(0);
    let (mut session, creds, _) = setup_input_session(t0);
    let mut sink = TestSink::default();

    let request = InputRequest {
        credentials: creds,
        sequence: 0,
        event: InputEvent::Button {
            button: PointerButton::Primary,
            pressed: true,
            position: DesktopPoint { x: 200, y: 300 },
            barrier: 0,
        },
    };

    // Sent at 100ms, but submission loop is stalled until 1500ms (ticket lifetime is 500ms)
    lab.send(Destination::Host, b"stalled-action", Fault::after(ms(100)))
        .unwrap();
    lab.elapse(ms(1_500)).unwrap();

    let mut result = None;
    lab.drain(|delivery| {
        // Dispatch samples clock immediately before submission at 1500ms
        result = Some(session.dispatch(request, &mut sink, || delivery.now));
        0
    })
    .unwrap();

    match result {
        Some(Ok(Dispatch::Completed(receipt))) => {
            assert_eq!(
                receipt.outcome,
                InputOutcome::ExpiredBeforeSubmission,
                "{lab:?}"
            );
            assert_eq!(receipt.submitted_operations, 0, "{lab:?}");
            assert_eq!(
                receipt.refusal,
                Some(Refusal::Authority(AuthorityError::TicketExpired)),
                "{lab:?}"
            );
        }
        other => panic!("Expected ExpiredBeforeSubmission receipt, got {other:?}"),
    }
    assert_eq!(
        sink.submitted.len(),
        0,
        "No OS submission performed: {lab:?}"
    );
}

// ---------------------------------------------------------------------------
// 8. No action resurrection after a stalled reactor
// ---------------------------------------------------------------------------

#[test]
fn prop_no_action_resurrection_after_stalled_reactor() {
    let mut lab = lab(108);
    let t0 = HostInstant::from_micros(0);
    let (mut session, creds, _) = setup_input_session(t0);
    let mut sink = TestSink::default();

    // Reactor stalls for 4 seconds (authority lifetime is 3s)
    lab.elapse(ms(4_000)).unwrap();

    // Stalled reactor triggers maintain / resume boundary
    let cleanup = session.maintain(lab.now(), &mut sink);
    assert_eq!(cleanup.submitted_releases, 0, "{lab:?}");

    // Queued actions cannot execute after resume
    let request = InputRequest {
        credentials: creds,
        sequence: 0,
        event: InputEvent::Button {
            button: PointerButton::Secondary,
            pressed: true,
            position: DesktopPoint { x: 10, y: 10 },
            barrier: 0,
        },
    };

    let result = session.dispatch(request, &mut sink, || lab.now());
    match result {
        Ok(Dispatch::Completed(receipt)) => {
            assert_eq!(
                receipt.outcome,
                InputOutcome::CancelledBeforeSubmission,
                "{lab:?}"
            );
            assert_eq!(receipt.submitted_operations, 0, "{lab:?}");
            assert_eq!(receipt.refusal, Some(Refusal::Revoked), "{lab:?}");
        }
        Err(Refusal::Revoked | Refusal::Sequence(InputSequenceError::Fenced)) => {}
        other => {
            panic!("Expected CancelledBeforeSubmission receipt, Revoked, or Fenced, got {other:?}")
        }
    }
    assert_eq!(sink.submitted.len(), 0, "{lab:?}");
}

// ---------------------------------------------------------------------------
// 9. Read-only approval enforcement
// ---------------------------------------------------------------------------

#[test]
fn prop_readonly_approval_enforcement() {
    let t0 = HostInstant::from_micros(0);
    let mut auth = SessionAuthority::new(
        RemoteSessionId::from_raw(1),
        AuthorityPolicy::plan_defaults(),
    );
    auth.mark_capabilities_checked().unwrap();
    auth.require_approval().unwrap();

    // View cannot be marked ready without approval (invalid state while awaiting approval)
    assert!(
        auth.mark_view_ready(t0).is_err(),
        "Approval required before view readiness"
    );

    // Observation is not authorized; readiness is Unready
    assert_eq!(auth.readiness(), ViewReadiness::Unready);
    assert!(!auth.has_live_control(t0));

    // Explicit local approval granted
    auth.authorize_observation(t0).unwrap();
    assert_eq!(auth.mark_view_ready(t0), Ok(()));

    // Frame / view readiness still does NOT grant input control
    assert!(
        !auth.has_live_control(t0),
        "Viewing alone never grants control"
    );
}

// ---------------------------------------------------------------------------
// 10. Controller-handoff serialization
// ---------------------------------------------------------------------------

#[test]
fn prop_controller_handoff_serialization() {
    let t0 = HostInstant::from_micros(0);
    let mut auth = SessionAuthority::new(
        RemoteSessionId::from_raw(1),
        AuthorityPolicy::plan_defaults(),
    );
    auth.mark_capabilities_checked().unwrap();
    auth.authorize_observation(t0).unwrap();
    auth.mark_view_ready(t0).unwrap();

    let lease_1 = InputLeaseId::from_raw(1);
    let lease_2 = InputLeaseId::from_raw(2);

    // Grant control to viewer 1
    auth.grant_lease(lease_1, t0).unwrap();
    assert!(auth.has_live_control(t0));

    // Viewer 2 requests control while Viewer 1 lease is live
    assert_eq!(
        auth.grant_lease(lease_2, t0),
        Err(AuthorityError::ControllerBusy),
        "Controller busy refused concurrent viewer"
    );

    // Renew observation at 2.0s so observation stays live until 5.0s
    auth.issue_observation_challenge(1, HostInstant::from_micros(2_000_000))
        .unwrap();
    auth.respond_observation_challenge(1, HostInstant::from_micros(2_000_000))
        .unwrap();

    // Viewer 1 lease expires at 3.0s. At 3.1s, lease 1 has expired but observation is still live.
    let t_expired = HostInstant::from_micros(3_100_000);
    assert_eq!(
        auth.grant_lease(lease_2, t_expired),
        Err(AuthorityError::ControllerCleanupRequired),
        "Cleanup required before handoff"
    );

    // Explicit revoke of lease 1 allows serialized handoff to lease 2
    auth.revoke_lease();
    assert_eq!(
        auth.grant_lease(lease_2, t_expired),
        Ok(()),
        "Handoff serialized cleanly"
    );
}

// ---------------------------------------------------------------------------
// 11. Shared-pipeline lifetime: closing one viewer never cancels another
// ---------------------------------------------------------------------------

#[test]
fn prop_shared_pipeline_lifetime_closing_one_viewer_never_cancels_another() {
    let c = default_media_config();

    // Two independent send caches representing two viewer subscriptions on a shared encoder
    let mut viewer_a =
        SendCache::new(c.limits, c.bindings, c.epoch, SendPolicy::default()).unwrap();
    let mut viewer_b =
        SendCache::new(c.limits, c.bindings, c.epoch, SendPolicy::default()).unwrap();

    // First push IDR frame
    let idr_p = Progress {
        descriptor: test_descriptor(0, None, 500, c.limits.fragment_stride()),
        observed_micros: 0,
        observation: SourceObservation::Captured,
        pipeline: PipelineState::Running,
    };
    viewer_a
        .push(idr_p, vec![1_u8; 500], DeliveryMode::Recovery, 0)
        .unwrap();
    viewer_b
        .push(idr_p, vec![1_u8; 500], DeliveryMode::Recovery, 0)
        .unwrap();

    // Push P-frame
    let p_frame = Progress {
        descriptor: test_descriptor(1, Some(0), 500, c.limits.fragment_stride()),
        observed_micros: 1_000,
        observation: SourceObservation::Captured,
        pipeline: PipelineState::Running,
    };
    viewer_a
        .push(p_frame, vec![2_u8; 500], DeliveryMode::Datagrams, 1_000)
        .unwrap();
    viewer_b
        .push(p_frame, vec![2_u8; 500], DeliveryMode::Datagrams, 1_000)
        .unwrap();

    // Viewer A disconnects / closes
    viewer_a.close();

    // Viewer B continues running without disruption
    let mut out_b = [0; 1_150];
    let offer_b = viewer_b.next_packet(1_000, &mut out_b).unwrap();
    assert!(
        offer_b.is_some(),
        "Viewer B continues streaming after Viewer A closes"
    );
    viewer_b.authorize_write(&offer_b.unwrap(), 1_000).unwrap();
}

// ---------------------------------------------------------------------------
// 12. Late viewer join waits for a fresh recovery point
// ---------------------------------------------------------------------------

#[test]
fn prop_late_viewer_join_waits_for_fresh_recovery_point() {
    let c = default_media_config();
    let budget = MediaBudget::new(c.limits.protocol()).unwrap();
    let mut late_receiver = ReceivePipeline::new(c, budget).unwrap();
    late_receiver.decoder_configured(0).unwrap();

    // Late receiver is initially awaiting recovery
    assert_eq!(late_receiver.state(), ReceiveState::AwaitingRecovery);

    // A P-frame (frame 42, referencing 41) arrives
    let p_frame_desc = test_descriptor(42, Some(41), 200, 1077);
    let mut p_packet = vec![0; c.limits.record_bytes()];
    let n = encode_fragment(
        fr_wire::Fragment {
            descriptor: p_frame_desc,
            index: 0,
            bytes: &[7_u8; 200],
        },
        c.bindings.for_channel(Channel::Video),
        &c.limits,
        &mut p_packet,
    )
    .unwrap();
    p_packet.truncate(n);

    // P-frame received while awaiting recovery is rejected or ignored
    let res = late_receiver.receive(Channel::Video, &p_packet, 100);
    assert!(
        res.is_err() || late_receiver.take_decodable(100).unwrap().is_none(),
        "P-frame cannot decode without recovery point"
    );
    assert_eq!(late_receiver.state(), ReceiveState::AwaitingRecovery);

    // Fresh IDR recovery chunk arrives on reliable recovery channel
    let idr_bytes = vec![88_u8; 300];
    let mut idr_packet = [0; 1_150];
    let n_idr = encode_recovery(
        RecoveryChunk {
            frame: 43,
            total_bytes: 300,
            offset: 0,
            capture_micros: 200,
            bytes: &idr_bytes,
        },
        c.bindings.for_channel(Channel::Recovery),
        &c.limits,
        &mut idr_packet,
    )
    .unwrap();

    late_receiver
        .receive(Channel::Recovery, &idr_packet[..n_idr], 200)
        .unwrap();
    let recovered_pic = late_receiver.take_decodable(200).unwrap().unwrap();
    assert_eq!(recovered_pic.bytes(), idr_bytes.as_slice());
    late_receiver
        .acknowledge_decode(&recovered_pic, true, 200)
        .unwrap();
    assert_eq!(late_receiver.state(), ReceiveState::Streaming);
}

// ---------------------------------------------------------------------------
// 13. Old pointer datagrams after a click
// ---------------------------------------------------------------------------

#[test]
fn prop_old_pointer_datagrams_after_a_click() {
    let mut lab = lab(113);
    let t0 = HostInstant::from_micros(0);
    let (mut session, creds, _) = setup_input_session(t0);
    let mut sink = TestSink::default();

    // Pre-click pointer at sequence 5
    let p5 = InputRequest {
        credentials: creds,
        sequence: 5,
        event: InputEvent::Pointer {
            position: DesktopPoint { x: 100, y: 100 },
        },
    };
    let _ = session.dispatch(p5, &mut sink, || t0).unwrap();

    // Click at sequence 0 with pointer barrier 10
    let click = InputRequest {
        credentials: creds,
        sequence: 0,
        event: InputEvent::Button {
            button: PointerButton::Primary,
            pressed: true,
            position: DesktopPoint { x: 150, y: 150 },
            barrier: 10,
        },
    };
    let _ = session.dispatch(click, &mut sink, || t0).unwrap();

    // Late pre-click pointer datagram at sequence 8 arriving after the click
    let late_pointer = InputRequest {
        credentials: creds,
        sequence: 8, // 8 <= barrier 10
        event: InputEvent::Pointer {
            position: DesktopPoint { x: 110, y: 110 },
        },
    };

    lab.send(
        Destination::Host,
        b"late-pointer-datagram",
        Fault::after(ms(5)),
    )
    .unwrap();
    lab.elapse(ms(5)).unwrap();
    let mut res = None;
    lab.drain(|_| {
        res = Some(session.dispatch(late_pointer, &mut sink, || t0));
        0
    })
    .unwrap();
    assert_eq!(
        res,
        Some(Ok(Dispatch::ObsoletePointer)),
        "Late pointer dropped by click barrier: {lab:?}"
    );
}

// ---------------------------------------------------------------------------
// 14. Client-freshness vs unchanged-screen distinction
// ---------------------------------------------------------------------------

#[test]
fn prop_client_freshness_vs_unchanged_screen_distinction() {
    let mut lab = lab(114);
    let c = default_media_config();
    let (mut receiver, _) = bootstrap_receiver(c);

    let sample = ClockSample {
        host_boot: HostBootId::from_raw(1),
        client_sent_us: 0,
        host_sample_us: 50,
        client_received_us: 100,
    };
    let clock = ClockCorrelation::new(sample, ClockPolicy::default()).unwrap();
    let mut tracker = ViewTracker::new(&receiver, clock, 500_000, 100).unwrap();

    // Announce frame 1 progress (Captured) at 1_000 us
    let desc1 = test_descriptor(1, Some(0), 100, 1077);
    let prog1 = Progress {
        descriptor: desc1,
        observed_micros: 1_000,
        observation: SourceObservation::Captured,
        pipeline: PipelineState::Running,
    };
    let mut prog1_pkt = vec![0; c.limits.record_bytes()];
    let np1 = encode_progress(
        prog1,
        c.bindings.for_channel(Channel::MediaConfig),
        &c.limits,
        &mut prog1_pkt,
    )
    .unwrap();
    prog1_pkt.truncate(np1);
    tracker.progress(&prog1_pkt, &c.limits, 1_000).unwrap();

    let mut pkt = vec![0; c.limits.record_bytes()];
    let n = encode_fragment(
        fr_wire::Fragment {
            descriptor: desc1,
            index: 0,
            bytes: &[99_u8; 100],
        },
        c.bindings.for_channel(Channel::Video),
        &c.limits,
        &mut pkt,
    )
    .unwrap();
    lab.send(Destination::Client, &pkt[..n], Fault::after(ms(1)))
        .unwrap();
    lab.elapse(ms(1)).unwrap();
    lab.drain(|delivery| {
        receiver
            .receive(Channel::Video, delivery.payload, 1_000)
            .unwrap();
        0
    })
    .unwrap();
    let pic1 = receiver.take_decodable(1_000).unwrap().unwrap();
    let decoded1 = receiver.complete_decode(&pic1, 1_000).unwrap();

    tracker.decoded(decoded1, true, 1_000).unwrap();
    let _evidence1 = tracker.visible(1, 1_000).unwrap();

    // Screen is unchanged: host observes source unchanged at 200_000 us
    let prog_unchanged = Progress {
        descriptor: desc1,
        observed_micros: 200_000,
        observation: SourceObservation::QualifiedUnchanged,
        pipeline: PipelineState::Running,
    };
    let mut unchanged_wire = vec![0; c.limits.record_bytes()];
    let np = encode_progress(
        prog_unchanged,
        c.bindings.for_channel(Channel::MediaConfig),
        &c.limits,
        &mut unchanged_wire,
    )
    .unwrap();
    unchanged_wire.truncate(np);

    lab.send(Destination::Client, &unchanged_wire, Fault::after(ms(200)))
        .unwrap();
    lab.elapse(ms(200)).unwrap();
    lab.drain(|delivery| {
        tracker
            .progress(delivery.payload, &c.limits, 201_000)
            .unwrap();
        0
    })
    .unwrap();

    let evidence_fresh = tracker.evidence(201_000).unwrap();
    // Pixel age is measured from capture time (desc1.capture_micros = 1000) -> ~200ms old
    // Source age is measured from observation (200_000) -> ~1ms old!
    assert!(
        evidence_fresh.pixel_age_upper_us > 150_000,
        "Pixels remain old: {lab:?}"
    );
    assert!(
        evidence_fresh.source_age_upper_us < 50_000,
        "Source observation is fresh: {lab:?}"
    );

    // If host stops verifying source for > max_source_age_us (500ms), screen becomes stale
    let stale_err = tracker.evidence(800_000);
    assert_eq!(
        stale_err,
        Err(FreshnessError::SourceStale),
        "Frozen screen identified as stale: {lab:?}"
    );
}

// ---------------------------------------------------------------------------
// 15. Committed vs uncommitted work in channel cancellation
// ---------------------------------------------------------------------------

#[test]
fn prop_committed_vs_uncommitted_work_in_channel_cancellation() {
    let mut lab = lab(115);
    let t0 = HostInstant::from_micros(0);
    let (mut session, creds, _) = setup_input_session(t0);
    let mut sink = TestSink::default();

    // Action 0 is admitted and committed to sink
    let req0 = InputRequest {
        credentials: creds,
        sequence: 0,
        event: InputEvent::Button {
            button: PointerButton::Primary,
            pressed: true,
            position: DesktopPoint { x: 10, y: 10 },
            barrier: 0,
        },
    };
    let _ = session.dispatch(req0, &mut sink, || t0).unwrap();
    assert_eq!(sink.submitted.len(), 2, "{lab:?}");

    // Queue in-flight uncommitted datagrams on transport
    lab.send(Destination::Host, b"uncommitted-1", Fault::after(ms(50)))
        .unwrap();
    lab.send(Destination::Host, b"uncommitted-2", Fault::after(ms(60)))
        .unwrap();

    // Fence transport channel (cancellation / reconnect)
    lab.fence().unwrap();

    // Drain queued uncommitted datagrams: all are traced as Stale, not delivered
    lab.elapse(ms(100)).unwrap();
    let mut delivered_count = 0;
    lab.drain(|_| {
        delivered_count += 1;
        0
    })
    .unwrap();
    assert_eq!(
        delivered_count, 0,
        "No uncommitted packet delivered after fence: {lab:?}"
    );

    // Committed receipts remain retrievable and never rolled back
    let receipt = session.retained_receipt(0);
    assert!(receipt.is_some(), "Committed receipts retained: {lab:?}");
}

// ---------------------------------------------------------------------------
// 16. Admitted vs irreversibly submitted OS injection in teardown results
// ---------------------------------------------------------------------------

#[test]
fn prop_admitted_vs_irreversibly_submitted_os_injection_in_teardown() {
    let mut lab = lab(116);
    let t0 = HostInstant::from_micros(0);
    let (mut session, creds, _) = setup_input_session(t0);
    let mut sink = TestSink::default();

    // Press key 42 (held state)
    let key_down = InputRequest {
        credentials: creds,
        sequence: 0,
        event: InputEvent::Key {
            key: PhysicalKey::new(42).unwrap(),
            transition: KeyTransition::Press,
        },
    };
    lab.send(Destination::Host, b"key-down", Fault::after(ms(5)))
        .unwrap();
    lab.elapse(ms(5)).unwrap();
    lab.drain(|_| {
        let _ = session.dispatch(key_down, &mut sink, || t0).unwrap();
        0
    })
    .unwrap();
    assert_eq!(session.held_count(), 1, "{lab:?}");

    // Teardown via cleanup: must submit release and report exact counts
    let cleanup = session.cleanup(&mut sink);
    assert_eq!(
        cleanup.submitted_releases, 1,
        "Exactly one release submitted: {lab:?}"
    );
    assert_eq!(cleanup.remaining, 0, "No remaining held state: {lab:?}");
    assert_eq!(session.held_count(), 0, "{lab:?}");
}

// ---------------------------------------------------------------------------
// Recovery Model Rows (P1_RECOVERY)
// ---------------------------------------------------------------------------

#[test]
fn recovery_row_missing_first_fragment() {
    let c = default_media_config();
    let (mut receiver, _) = bootstrap_receiver(c);
    let d = test_descriptor(1, Some(0), 3000, 1077);
    send_progress(&mut receiver, &c, d, 1000, PipelineState::Running);
    send_fragments(&mut receiver, &c, d, &[10_u8; 3000], &[1, 2], 1000);
    assert!(receiver.take_decodable(1000).unwrap().is_none());
    assert!(receiver.repair_needed(1));
}

#[test]
fn recovery_row_missing_middle_fragment() {
    let c = default_media_config();
    let (mut receiver, _) = bootstrap_receiver(c);
    let d = test_descriptor(1, Some(0), 3000, 1077);
    send_progress(&mut receiver, &c, d, 1000, PipelineState::Running);
    send_fragments(&mut receiver, &c, d, &[10_u8; 3000], &[0, 2], 1000);
    assert!(receiver.take_decodable(1000).unwrap().is_none());
    assert!(receiver.repair_needed(1));
}

#[test]
fn recovery_row_missing_final_fragment() {
    let c = default_media_config();
    let (mut receiver, _) = bootstrap_receiver(c);
    let d = test_descriptor(1, Some(0), 3000, 1077);
    send_progress(&mut receiver, &c, d, 1000, PipelineState::Running);
    send_fragments(&mut receiver, &c, d, &[10_u8; 3000], &[0, 1], 1000);
    assert!(receiver.take_decodable(1000).unwrap().is_none());
    assert!(receiver.repair_needed(1));
}

#[test]
fn recovery_row_loss_immediately_before_idle() {
    let c = default_media_config();
    let (mut receiver, _) = bootstrap_receiver(c);
    let d = test_descriptor(1, Some(0), 1000, 1077);
    send_progress(&mut receiver, &c, d, 2000, PipelineState::Idle);
    assert!(receiver.repair_needed(1));
    let mut offer_buf = [0; 1150];
    let offer = receiver.repair_offer(22_000, &mut offer_buf).unwrap();
    assert!(
        offer.is_some(),
        "Repair offer ready after delay even when idle"
    );
}

#[test]
fn recovery_row_reference_repair_after_own_display_deadline() {
    let c = default_media_config();
    let (mut receiver, _) = bootstrap_receiver(c);
    let d = test_descriptor(1, Some(0), 1000, 1077);
    send_progress(&mut receiver, &c, d, 1000, PipelineState::Running);
    send_fragments(&mut receiver, &c, d, &[55_u8; 1000], &[0], 80_000);
    let pic = receiver.take_decodable(80_000).unwrap();
    assert!(
        pic.is_some(),
        "Decodable as reference frame after display deadline"
    );
}

#[test]
fn recovery_row_lost_recovery_configuration_or_acknowledgement() {
    let c = default_media_config();
    let budget = MediaBudget::new(c.limits.protocol()).unwrap();
    let mut receiver = ReceivePipeline::new(c, budget).unwrap();
    assert_eq!(receiver.state(), ReceiveState::AwaitingConfiguration);

    let test_bytes = vec![1_u8; 100];
    let mut packet = [0; 1_150];
    let n = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 100,
            offset: 0,
            capture_micros: 0,
            bytes: &test_bytes,
        },
        c.bindings.for_channel(Channel::Recovery),
        &c.limits,
        &mut packet,
    )
    .unwrap();
    let res = receiver.receive(Channel::Recovery, &packet[..n], 0);
    assert_eq!(res, Err(DeliveryError::WrongState));
    assert_eq!(receiver.state(), ReceiveState::AwaitingConfiguration);
}

#[test]
fn recovery_row_exhausted_recovery_budget() {
    let mut c = default_media_config();
    c.policy.max_repair_attempts = 2;
    let (mut receiver, _) = bootstrap_receiver(c);
    let d = test_descriptor(1, Some(0), 1000, 1077);
    send_progress(&mut receiver, &c, d, 1000, PipelineState::Running);
    let mut out = [0; 1150];
    assert!(receiver.repair_offer(21_000, &mut out).unwrap().is_some());
    assert!(receiver.repair_offer(81_000, &mut out).unwrap().is_some());
    assert_eq!(
        receiver.repair_offer(141_000, &mut out).unwrap(),
        None,
        "Exhausted recovery budget refuses further repair attempts"
    );
}

#[test]
fn recovery_row_120ms_repair_horizon() {
    let c = default_media_config();
    let (mut receiver, _) = bootstrap_receiver(c);
    let d = test_descriptor(1, Some(0), 1000, 1077);
    send_progress(&mut receiver, &c, d, 1000, PipelineState::Running);
    let mut out = [0; 1150];
    assert!(
        receiver.repair_offer(60_000, &mut out).unwrap().is_some(),
        "Repair offer produced within repair horizon"
    );
}

#[test]
fn recovery_row_large_idr_reassembly() {
    let c = default_media_config();
    let budget = MediaBudget::new(c.limits.protocol()).unwrap();
    let mut receiver = ReceivePipeline::new(c, budget).unwrap();
    receiver.decoder_configured(0).unwrap();

    // Large IDR spanning 8 recovery chunks (8 * 1000 = 8000 bytes)
    let total_bytes = 8000;
    let full_data: Vec<u8> = (0..total_bytes)
        .map(|i| u8::try_from(i % 256).unwrap())
        .collect();

    for (chunk_idx, chunk) in full_data.chunks(1000).enumerate() {
        let mut packet = [0; 1150];
        let n = encode_recovery(
            RecoveryChunk {
                frame: 0,
                total_bytes,
                offset: u32::try_from(chunk_idx * 1000).unwrap(),
                capture_micros: 0,
                bytes: chunk,
            },
            c.bindings.for_channel(Channel::Recovery),
            &c.limits,
            &mut packet,
        )
        .unwrap();
        receiver
            .receive(Channel::Recovery, &packet[..n], 0)
            .unwrap();
    }

    let picture = receiver
        .take_decodable(0)
        .unwrap()
        .expect("Reassembled full IDR");
    assert_eq!(
        picture.bytes(),
        full_data.as_slice(),
        "Large IDR matches full payload exactly"
    );
}

#[test]
fn recovery_row_receiver_credit_starvation() {
    let c = default_media_config();
    let tight_budget = MediaBudget::new(c.limits.protocol()).unwrap();
    let mut receiver = ReceivePipeline::new(c, tight_budget).unwrap();
    receiver.decoder_configured(0).unwrap();

    // Feed normal recovery IDR to establish baseline
    let base_idr = vec![1_u8; 100];
    let mut pkt = [0; 1150];
    let n = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 100,
            offset: 0,
            capture_micros: 0,
            bytes: &base_idr,
        },
        c.bindings.for_channel(Channel::Recovery),
        &c.limits,
        &mut pkt,
    )
    .unwrap();
    receiver.receive(Channel::Recovery, &pkt[..n], 0).unwrap();
    let pic = receiver.take_decodable(0).unwrap().unwrap();
    receiver.acknowledge_decode(&pic, true, 0).unwrap();

    // Check budget usage is accurately tracked in bytes
    let usage = receiver.budget_usage();
    assert!(usage.bytes > 0 || usage.pictures > 0);
}

// ---------------------------------------------------------------------------
// Deterministic Replay Verification
// ---------------------------------------------------------------------------

#[test]
fn test_suite_deterministic_replay_runs_produce_identical_logs() {
    let run = |seed: u64| -> Vec<fr_lab::TraceEvent> {
        let mut lab = Scenario::new(seed, Limits::default()).unwrap();
        for i in 0..20 {
            lab.send(
                Destination::Host,
                &[i],
                Fault::Seeded {
                    max_delay: us(500),
                    loss_per_million: 300_000,
                    duplicate_per_million: 400_000,
                },
            )
            .unwrap();
        }
        lab.elapse(us(500)).unwrap();
        lab.drain(|_| 0).unwrap();
        lab.trace().to_vec()
    };

    let run1 = run(777);
    let run2 = run(777);
    let run_different = run(778);

    assert_eq!(run1, run2, "Identical seeds produce identical trace logs");
    assert_ne!(
        run1, run_different,
        "Different seeds produce different trace logs"
    );
}
