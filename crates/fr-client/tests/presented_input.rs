//! Actual codecs and policy owners, with explicitly simulated media/native
//! callbacks. Real process/HEVC/X11 composition is in the native integration.
use fr_client::input::presentation::{Error, PresentedInput};
use fr_client::input::{self, Action, ClientInstant, InputClient, Policy, ResultEvent, StopReason};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_submission::{
        Capabilities, Capability, Dispatch, InputSession, InputSink, Operation, PlatformError,
        Submission,
    },
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_media::{
    delivery::*,
    freshness::{self, ClockCorrelation, ClockPolicy, ClockSample},
};
use fr_wire::{
    input::{InputDelivery, InputDirection, decode_input},
    input_result::*,
    *,
};
fn credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}
fn caps() -> Capabilities {
    Capabilities::default()
        .with(Capability::Absolute)
        .with(Capability::Keys)
}
fn bounds() -> InputBounds {
    InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap()
}
fn setup() -> (ReceivePipeline, PresentedInput, MediaLimits) {
    let c = credentials();
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap();
    let mut receiver = ReceivePipeline::new(
        ReceiveConfig {
            limits,
            bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
            epoch: MediaEpoch {
                configuration: c.view.configuration,
                recovery: c.view.recovery,
            },
            policy: ReceivePolicy::default(),
        },
        MediaBudget::new(limits.protocol()).unwrap(),
    )
    .unwrap();
    receiver.decoder_configured(10_000).unwrap();
    let input = InputClient::new(
        c,
        7,
        bounds(),
        caps(),
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(10_000),
    )
    .unwrap();
    let clock = ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(1),
            client_sent_us: 0,
            host_sample_us: 1_000_000,
            client_received_us: 10_000,
        },
        ClockPolicy {
            drift_ppm: 0,
            ..ClockPolicy::default()
        },
    )
    .unwrap();
    let mut input = PresentedInput::new(input, &receiver, clock, ClientInstant(10_000)).unwrap();
    input
        .confirm_mapping(c.session, c.view, ClientInstant(10_000))
        .unwrap();
    (receiver, input, limits)
}
fn progress(
    input: &mut PresentedInput,
    limits: MediaLimits,
    d: FrameDescriptor,
    at: u64,
    observation: SourceObservation,
    now: u64,
) -> Result<(), Error> {
    let mut out = [0; 1150];
    let n = encode_progress(
        Progress {
            descriptor: d,
            observed_micros: at,
            observation,
            pipeline: PipelineState::Running,
        },
        3,
        &limits,
        &mut out,
    )
    .unwrap();
    input.progress(&out[..n], &limits, ClientInstant(now))
}
fn decoded(
    receiver: &mut ReceivePipeline,
    input: &mut PresentedInput,
    limits: MediaLimits,
    when: u64,
) -> FrameDescriptor {
    let mut out = [0; 1150];
    let n = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 4,
            offset: 0,
            capture_micros: 1_005_000,
            bytes: b"data",
        },
        2,
        &limits,
        &mut out,
    )
    .unwrap();
    receiver
        .receive(Channel::Recovery, &out[..n], 20_000)
        .unwrap();
    let unit = receiver.take_decodable(20_000).unwrap().unwrap();
    let descriptor = unit.descriptor();
    progress(
        input,
        limits,
        descriptor,
        1_005_000,
        SourceObservation::Captured,
        20_000,
    )
    .unwrap();
    input
        .decoded(
            receiver.complete_decode(&unit, when).unwrap(),
            true,
            ClientInstant(when),
        )
        .unwrap();
    descriptor
}
/// A stale view suspends input at exactly `at` (plan 11.3): the grant is not
/// stopped, `tick` reports not ready, and an action is refused unencoded.
fn suspended_at(input: &mut PresentedInput, at: u64) {
    assert_eq!(input.tick(ClientInstant(at)), Ok(false));
    assert_eq!(input.stopped(), None);
    assert_eq!(input.suspended_since(), Some(ClientInstant(at)));
    let mut out = [0xAA; 512];
    assert_eq!(
        input.action(key(), &mut out, ClientInstant(at)),
        Err(Error::Input(input::Error::ViewSuspended))
    );
    assert!(out.iter().all(|v| *v == 0xAA));
}
fn key() -> Action<'static> {
    Action::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: KeyTransition::Press,
    }
}
fn shown() -> (
    ReceivePipeline,
    PresentedInput,
    MediaLimits,
    FrameDescriptor,
) {
    let (mut receiver, mut input, limits) = setup();
    let descriptor = decoded(&mut receiver, &mut input, limits, 21_000);
    input.visible(0, ClientInstant(22_000)).unwrap();
    (receiver, input, limits, descriptor)
}
#[test]
fn real_records_need_both_decode_and_visibility_before_input_encoding() {
    let (mut receiver, mut input, limits) = setup();
    let mut out = [0xAA; 512];
    assert_eq!(
        input.action(key(), &mut out, ClientInstant(11_000)),
        Err(Error::Input(input::Error::NoPresentedView))
    );
    assert!(out.iter().all(|v| *v == 0xAA));
    decoded(&mut receiver, &mut input, limits, 21_000);
    assert_eq!(
        input.action(key(), &mut out, ClientInstant(21_001)),
        Err(Error::Input(input::Error::NoPresentedView))
    );
    let evidence = input.visible(0, ClientInstant(22_000)).unwrap();
    assert_eq!(evidence.source_age_upper_us, 17_000);
    let encoded = input
        .action(key(), &mut out, ClientInstant(23_000))
        .unwrap();
    let request = decode_input(
        &out[..encoded.bytes],
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(request.sequence, 0);
    assert_eq!(request.credentials, credentials());
}
#[test]
fn static_observations_extend_source_deadline_without_new_video_or_polling_renewal() {
    let (_receiver, mut input, limits, d) = shown();
    progress(
        &mut input,
        limits,
        d,
        1_190_000,
        SourceObservation::QualifiedUnchanged,
        200_000,
    )
    .unwrap();
    assert!(input.tick(ClientInstant(300_000)).unwrap());
    assert!(input.tick(ClientInstant(439_999)).unwrap());
    suspended_at(&mut input, 440_000);
    // A later fresh observation alone does not resume input: a ticket issued
    // after the host's own lapse is also required.
    progress(
        &mut input,
        limits,
        d,
        1_441_000,
        SourceObservation::QualifiedUnchanged,
        441_000,
    )
    .unwrap();
    assert_eq!(input.tick(ClientInstant(442_000)), Ok(false));
    assert_eq!(input.suspended_since(), Some(ClientInstant(440_000)));
}
#[test]
fn unknown_source_with_zero_timestamp_suspends_without_waiting_for_old_pixels_to_expire() {
    let (_receiver, mut input, limits, d) = shown();
    progress(&mut input, limits, d, 0, SourceObservation::Unknown, 23_000).unwrap();
    suspended_at(&mut input, 23_000);
    // An opaque ticket is not one issued after the host's lapse: still suspended.
    input
        .ticket(InputTicketId::from_raw(9), ClientInstant(23_001))
        .unwrap();
    assert_eq!(input.tick(ClientInstant(23_002)), Ok(false));
    assert_eq!(input.stopped(), None);
}
#[test]
fn receiver_failure_is_checked_at_the_next_action_without_a_new_callback() {
    let (mut receiver, mut input, _, _) = shown();
    receiver.close();
    let mut out = [0xAA; 512];
    assert!(
        input
            .action(key(), &mut out, ClientInstant(23_000))
            .is_err()
    );
    assert_eq!(input.stopped(), Some(StopReason::ViewStale));
    assert!(out.iter().all(|v| *v == 0xAA));
}
#[test]
fn late_decoder_submission_never_becomes_an_input_ready_view() {
    let (mut receiver, mut input, limits) = setup();
    decoded(&mut receiver, &mut input, limits, 70_000);
    assert_eq!(
        input.visible(0, ClientInstant(70_000)),
        Err(Error::Media(freshness::Error::QueueExpired))
    );
    assert!(!input.tick(ClientInstant(70_001)).unwrap());
    assert!(
        input
            .pointer(
                DesktopPoint { x: 2, y: 2 },
                &mut [0; 512],
                ClientInstant(70_002)
            )
            .is_err()
    );
}
struct Sink;
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, _: Operation) -> Submission {
        Submission::Submitted
    }
}
#[test]
fn hidden_view_preserves_late_real_core_receipts_without_restoring_input() {
    let (_receiver, mut input, _, _) = shown();
    let mut out = [0; 512];
    let encoded = input
        .action(key(), &mut out, ClientInstant(23_000))
        .unwrap();
    let request = decode_input(
        &out[..encoded.bytes],
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    let c = credentials();
    let mut authority = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority
        .authorize_observation(HostInstant::ORIGIN)
        .unwrap();
    authority.mark_view_ready(HostInstant::ORIGIN).unwrap();
    authority.grant_lease(c.lease, HostInstant::ORIGIN).unwrap();
    authority
        .issue_input_ticket(c.lease, c.ticket, HostInstant::ORIGIN)
        .unwrap();
    let mut host = InputSession::new(authority, c, bounds(), caps(), HostInstant::ORIGIN).unwrap();
    let Dispatch::Completed(receipt) = host
        .dispatch(request, &mut Sink, || HostInstant::ORIGIN)
        .unwrap()
    else {
        panic!("receipt required")
    };
    let result = InputResult::from_receipt(
        ResultBinding {
            channel: 7,
            session: c.session,
            lease: c.lease,
        },
        SequenceSpace::Action,
        receipt,
    )
    .unwrap();
    let n = encode_input_result(
        result,
        &mut out,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    input.hidden();
    assert_eq!(
        input.result(&out[..n], ClientInstant(30_000)).unwrap(),
        ResultEvent::Completed(result)
    );
    assert_eq!(input.pending_actions(), 0);
    assert_eq!(input.stopped(), Some(StopReason::FocusLost));
    assert!(input.visible(0, ClientInstant(30_001)).is_err());
}
#[test]
fn new_unpresented_picture_cannot_reset_existing_view_age() {
    let (_receiver, mut input, limits, mut d) = shown();
    d.frame = 1;
    d.reference = Some(0);
    d.capture_micros = 1_230_000;
    progress(
        &mut input,
        limits,
        d,
        1_230_000,
        SourceObservation::Captured,
        240_000,
    )
    .unwrap();
    assert!(input.tick(ClientInstant(254_999)).unwrap());
    suspended_at(&mut input, 255_000);
}
#[test]
fn generation_change_cannot_reuse_the_old_presented_grant() {
    let (mut receiver, mut input, _, _) = shown();
    receiver
        .replace(
            MediaEpoch {
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL.next().unwrap(),
            },
            MediaBindings::new(5, 6, 7, 8).unwrap(),
            23_000,
        )
        .unwrap();
    assert!(input.tick(ClientInstant(23_001)).is_err());
    assert!(
        input
            .action(key(), &mut [0; 512], ClientInstant(23_002))
            .is_err()
    );
}

/// A control challenge on renewal channel 11, due at 4 s.
fn control_challenge(nonce: u128) -> Vec<u8> {
    let mut bytes = [0; authority::MAX_AUTHORITY_BYTES];
    let n = authority::encode(
        authority::Message::Challenge {
            scope: authority::Scope::Control(credentials().lease),
            nonce,
            deadline_micros: 4_000_000,
        },
        authority::Binding {
            channel: 11,
            session: credentials().session,
        },
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes[..n].to_vec()
}
#[test]
fn control_renewal_continues_while_a_stale_view_suspends_input() {
    let (_receiver, mut input, limits, d) = shown();
    input
        .enable_control_renewal(11, ClientInstant(22_000))
        .unwrap();
    progress(&mut input, limits, d, 0, SourceObservation::Unknown, 23_000).unwrap();
    suspended_at(&mut input, 23_000);
    // The lease stays alive while suspended: answering its challenge is
    // renewal, not input, and neither resumes input nor ends the grant.
    input
        .accept_control_challenge(&control_challenge(7), ClientInstant(23_001))
        .unwrap();
    assert!(
        input
            .pending_control_response(ClientInstant(23_002))
            .unwrap()
            .is_some()
    );
    input.control_response_sent(ClientInstant(23_003)).unwrap();
    assert_eq!(input.stopped(), None);
    assert_eq!(input.suspended_since(), Some(ClientInstant(23_000)));
    assert_eq!(input.tick(ClientInstant(23_004)), Ok(false));
}
#[test]
fn lost_media_receiver_invalidates_a_pending_control_response_before_send() {
    let (mut receiver, mut input, limits) = setup();
    decoded(&mut receiver, &mut input, limits, 30_000);
    input.visible(0, ClientInstant(30_000)).unwrap();
    input
        .enable_control_renewal(11, ClientInstant(30_000))
        .unwrap();
    input
        .accept_control_challenge(&control_challenge(7), ClientInstant(30_000))
        .unwrap();
    assert!(
        input
            .pending_control_response(ClientInstant(30_000))
            .unwrap()
            .is_some()
    );
    drop(receiver);
    assert!(
        input
            .pending_control_response(ClientInstant(30_001))
            .is_err()
    );
    assert!(input.stopped().is_some());
    assert_eq!(input.control_response_deadline(), None);
}
#[test]
fn the_view_bound_follows_the_measured_path_up_to_its_ceiling() {
    // Host observation at client time 190 ms, 20 ms old when it arrives.
    for (rtt, stale_at) in [
        (None, 440_000),
        (Some(120_000), 800_000),
        (Some(10_000_000), 1_190_000),
    ] {
        let (_receiver, mut input, limits, d) = shown();
        input.follow_path_rtt(rtt).unwrap();
        progress(
            &mut input,
            limits,
            d,
            1_190_000,
            SourceObservation::QualifiedUnchanged,
            200_000,
        )
        .unwrap();
        assert!(input.tick(ClientInstant(stale_at - 1)).unwrap(), "{rtt:?}");
        suspended_at(&mut input, stale_at);
    }
}
#[test]
fn a_faster_path_applies_the_smaller_bound_to_the_viewer_at_once() {
    let (_receiver, mut input, limits, d) = shown();
    input.follow_path_rtt(Some(120_000)).unwrap();
    progress(
        &mut input,
        limits,
        d,
        1_190_000,
        SourceObservation::QualifiedUnchanged,
        200_000,
    )
    .unwrap();
    // The RTT estimate is gone: the viewer's tracker checks the base again at
    // once, so the unverified view is stale at the base deadline. Gating input
    // earlier than the path allowed is the conservative direction.
    input.follow_path_rtt(None).unwrap();
    assert!(input.tick(ClientInstant(439_999)).unwrap());
    suspended_at(&mut input, 440_000);
}
/// A reference recovery abandons an accepted dependent picture, which fences
/// the old receiver scope (`cancel_decode`).
fn abandon_dependent_picture(receiver: &mut ReceivePipeline, limits: MediaLimits) {
    let next = FrameDescriptor {
        frame: 1,
        reference: Some(0),
        total_bytes: 4,
        stride: 4,
        capture_micros: 1_012_000,
    };
    let mut out = [0; 1150];
    let n = encode_progress(
        Progress {
            descriptor: next,
            observed_micros: next.capture_micros,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        3,
        &limits,
        &mut out,
    )
    .unwrap();
    receiver
        .receive(Channel::MediaConfig, &out[..n], 23_200)
        .unwrap();
    let n = encode_fragment(
        Fragment {
            descriptor: next,
            index: 0,
            bytes: b"next",
        },
        1,
        &limits,
        &mut out,
    )
    .unwrap();
    receiver.receive(Channel::Video, &out[..n], 23_200).unwrap();
    let abandoned = receiver.take_decodable(23_200).unwrap().unwrap();
    abandoned.cancel_decode();
}
/// The recovered generation's IDR (bindings 11-14), decoded and shown.
fn show_recovered_idr(
    receiver: &mut ReceivePipeline,
    input: &mut PresentedInput,
    limits: MediaLimits,
) {
    receiver.decoder_configured(25_000).unwrap();
    let mut out = [0; 1150];
    let n = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 4,
            offset: 0,
            capture_micros: 1_014_000,
            bytes: b"data",
        },
        12,
        &limits,
        &mut out,
    )
    .unwrap();
    receiver
        .receive(Channel::Recovery, &out[..n], 25_000)
        .unwrap();
    let unit = receiver.take_decodable(25_000).unwrap().unwrap();
    let n = encode_progress(
        Progress {
            descriptor: unit.descriptor(),
            observed_micros: 1_014_000,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        13,
        &limits,
        &mut out,
    )
    .unwrap();
    input
        .progress(&out[..n], &limits, ClientInstant(25_000))
        .unwrap();
    input
        .decoded(
            receiver.complete_decode(&unit, 26_000).unwrap(),
            true,
            ClientInstant(26_000),
        )
        .unwrap();
    input.visible(0, ClientInstant(27_000)).unwrap();
}
#[test]
fn a_reference_recovery_rebinds_the_suspended_grant_to_the_recovered_receiver() {
    let (mut receiver, mut input, limits, _) = shown();
    input.expect_reference_recovery();
    // The loss is found while the view is still live: the abandoned decode
    // fences the old receiver scope before any recovery begins. The grant's
    // next tick suspends it instead of ending it on that stale tracker.
    abandon_dependent_picture(&mut receiver, limits);
    assert_eq!(input.tick(ClientInstant(23_000)), Ok(false));
    assert_eq!(input.stopped(), None);
    assert_eq!(input.suspended_since(), Some(ClientInstant(23_000)));
    // Entering the recovery is idempotent. Ticks and clock updates leave the
    // grant suspended without consulting the fenced tracker.
    input.suspend_for_recovery(ClientInstant(23_100)).unwrap();
    assert_eq!(input.tick(ClientInstant(23_300)), Ok(false));
    let later = ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(1),
            client_sent_us: 23_000,
            host_sample_us: 1_013_000,
            client_received_us: 23_400,
        },
        ClockPolicy {
            drift_ppm: 0,
            ..ClockPolicy::default()
        },
    )
    .unwrap();
    input.synchronize(later, ClientInstant(23_400)).unwrap();
    assert_eq!(input.stopped(), None);
    // The receiver installs the recovery's next generation and fresh bindings.
    let next = RecoveryGeneration::INITIAL.next().unwrap();
    receiver
        .replace(
            MediaEpoch {
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: next,
            },
            MediaBindings::new(11, 12, 13, 14).unwrap(),
            24_000,
        )
        .unwrap();
    assert!(input.check_receiver(&receiver).is_err());
    input
        .follow_recovery(&receiver, ClientInstant(24_000))
        .unwrap();
    input.check_receiver(&receiver).unwrap();
    assert_eq!(input.input_view().recovery, next);
    assert_eq!(input.suspended_since(), Some(ClientInstant(23_000)));
    // The recovered IDR, decoded and shown, is the new generation's evidence.
    show_recovered_idr(&mut receiver, &mut input, limits);
    // Fresh evidence alone does not resume: that needs a ticket issued after
    // the host's own lapse. The grant is suspended, not ended.
    assert_eq!(input.tick(ClientInstant(27_001)), Ok(false));
    assert_eq!(input.stopped(), None);
}
