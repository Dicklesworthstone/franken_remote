//! Production wire/receiver/input owners; decode and visibility are explicit fixtures.
use fr_client::input::presentation::{Error, PresentedInput};
use fr_client::input::{self, Action, ClientInstant, InputClient, Policy, PresentedObservation};
use fr_core::{
    ids::*,
    input::*,
    input_submission::{Capabilities, Capability},
    limits::ProtocolLimits,
};
use fr_media::{
    delivery::*,
    freshness::{ClockCorrelation, ClockPolicy, ClockSample, ViewTracker},
};
use fr_wire::{
    input::{InputDelivery, InputDirection},
    input_ticket::{self, INPUT_TICKET_BYTES, Ticket},
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
fn clock() -> ClockCorrelation {
    ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(4),
            client_sent_us: 0,
            host_sample_us: 1_000_000,
            client_received_us: 10_000,
        },
        ClockPolicy {
            drift_ppm: 0,
            ..ClockPolicy::default()
        },
    )
    .unwrap()
}
fn receiver() -> (ReceivePipeline, MediaLimits) {
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap();
    let c = credentials();
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
    (receiver, limits)
}
fn shown(visible: bool) -> (ReceivePipeline, ViewTracker) {
    let (mut receiver, limits) = receiver();
    // The observation policy is deliberately less strict than the incoming input grant.
    let mut view = ViewTracker::new(&receiver, clock(), 500_000, 10_000).unwrap();
    let mut bytes = [0; 1150];
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
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::Recovery, &bytes[..n], 20_000)
        .unwrap();
    let unit = receiver.take_decodable(20_000).unwrap().unwrap();
    let n = encode_progress(
        Progress {
            descriptor: unit.descriptor(),
            observed_micros: 1_005_000,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        3,
        &limits,
        &mut bytes,
    )
    .unwrap();
    view.progress(&bytes[..n], &limits, 20_000).unwrap();
    view.decoded(
        receiver.complete_decode(&unit, 21_000).unwrap(),
        true,
        21_000,
    )
    .unwrap();
    if visible {
        view.visible(0, 22_000).unwrap();
    }
    drop(unit);
    (receiver, view)
}
fn input(c: InputCredentials) -> InputClient {
    let mut input = InputClient::new(
        c,
        7,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default().with(Capability::Keys),
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(30_000),
    )
    .unwrap();
    let mut bytes = [0; INPUT_TICKET_BYTES];
    let n = input_ticket::encode(
        Ticket {
            credentials: c,
            sequence: 0,
            issued_at_us: 1_000_000,
            expires_at_us: 1_500_000,
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    input
        .accept_ticket(&bytes[..n], clock(), ClientInstant(30_000))
        .unwrap();
    input
}
fn key() -> Action<'static> {
    Action::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: KeyTransition::Press,
    }
}
#[test]
fn existing_visible_frame_accepts_real_ticket_without_redecode_or_implicit_mapping() {
    let (receiver, view) = shown(true);
    let input = input(credentials());
    let ticket = input.ticket_deadline();
    let usage = receiver.budget_usage();
    let mut input =
        PresentedInput::from_view(input, &receiver, view, ClientInstant(30_000)).unwrap();
    assert_eq!(input.ticket_deadline(), ticket);
    assert_eq!(receiver.budget_usage(), usage);
    assert_eq!(input.pending_actions(), 0);
    let mut bytes = [0; 512];
    assert_eq!(
        input.action(key(), &mut bytes, ClientInstant(30_001)),
        Err(Error::Input(input::Error::MappingUnconfirmed))
    );
    input
        .confirm_mapping(
            credentials().session,
            credentials().view,
            ClientInstant(30_002),
        )
        .unwrap();
    let encoded = input
        .action(key(), &mut bytes, ClientInstant(30_003))
        .unwrap();
    let decoded = fr_wire::input::decode_input(
        &bytes[..encoded.bytes],
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(decoded.credentials, credentials());
    assert_eq!(decoded.sequence, 0);
}
#[test]
fn promotion_preserves_source_time_and_enforces_the_grants_stricter_age() {
    let (receiver, view) = shown(true);
    let mut input = PresentedInput::from_view(
        input(credentials()),
        &receiver,
        view,
        ClientInstant(200_000),
    )
    .unwrap();
    input
        .confirm_mapping(
            credentials().session,
            credentials().view,
            ClientInstant(200_000),
        )
        .unwrap();
    assert_eq!(
        input.view_deadline(ClientInstant(200_000)).unwrap(),
        ClientInstant(255_000)
    );
    assert!(input.tick(ClientInstant(254_999)).unwrap());
    assert!(input.tick(ClientInstant(255_000)).is_err());
}
#[test]
fn decode_submission_without_visibility_and_hidden_views_cannot_promote() {
    let (receiver, view) = shown(false);
    assert!(matches!(
        PresentedInput::from_view(input(credentials()), &receiver, view, ClientInstant(30_000)),
        Err(Error::Media(fr_media::freshness::Error::NotSubmitted))
    ));
    let (receiver, mut view) = shown(true);
    view.hide();
    assert!(matches!(
        PresentedInput::from_view(input(credentials()), &receiver, view, ClientInstant(30_000)),
        Err(Error::Media(fr_media::freshness::Error::NotSubmitted))
    ));
}
#[test]
fn equal_numeric_receiver_and_closed_original_cannot_borrow_presentation() {
    let (_original, view) = shown(true);
    let (foreign, _) = receiver();
    assert!(matches!(
        PresentedInput::from_view(input(credentials()), &foreign, view, ClientInstant(30_000)),
        Err(Error::Media(fr_media::freshness::Error::StaleBinding))
    ));
    let (mut receiver, view) = shown(true);
    receiver.close();
    assert!(matches!(
        PresentedInput::from_view(input(credentials()), &receiver, view, ClientInstant(30_000)),
        Err(Error::Media(fr_media::freshness::Error::StaleBinding))
    ));
}
#[test]
fn used_grant_and_mismatched_generation_cannot_reuse_existing_view() {
    let (receiver, view) = shown(true);
    let mut c = credentials();
    c.view.recovery = c.view.recovery.next().unwrap();
    assert!(matches!(
        PresentedInput::from_view(input(c), &receiver, view, ClientInstant(30_000)),
        Err(Error::ViewMismatch)
    ));
    let (receiver, view) = shown(true);
    let c = credentials();
    let mut client = input(c);
    client
        .presented(
            PresentedObservation {
                session: c.session,
                serial: 1,
                view: c.view,
                received_at: ClientInstant(30_000),
                source_age_upper_us: 0,
            },
            ClientInstant(30_000),
        )
        .unwrap();
    assert!(matches!(
        PresentedInput::from_view(client, &receiver, view, ClientInstant(30_000)),
        Err(Error::AlreadyUsed)
    ));
}
#[test]
fn late_promotion_cannot_restart_expired_source_freshness() {
    let (receiver, view) = shown(true);
    assert!(
        PresentedInput::from_view(
            input(credentials()),
            &receiver,
            view,
            ClientInstant(300_000)
        )
        .is_err()
    );
}

#[path = "view_promotion/clipboard.rs"]
mod clipboard;
