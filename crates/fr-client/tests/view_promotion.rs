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
    // The grant's stricter age suspends input at 255 ms (plan 11.3).
    assert_eq!(input.tick(ClientInstant(255_000)), Ok(false));
    assert_eq!(input.suspended_since(), Some(ClientInstant(255_000)));
    assert_eq!(input.stopped(), None);
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
fn a_grant_that_follows_a_slow_path_promotes_a_view_the_base_bound_refuses() {
    // At 300 ms this source is past the 250 ms base (the test below) but
    // inside a 120 ms RTT path's viewer bound, 250 + 3 x 120 ms.
    let (receiver, view) = shown(true);
    let mut client = input(credentials());
    assert_eq!(client.follow_path_rtt(Some(120_000)).unwrap(), 610_000);
    let mut input =
        PresentedInput::from_view(client, &receiver, view, ClientInstant(300_000)).unwrap();
    input
        .confirm_mapping(
            credentials().session,
            credentials().view,
            ClientInstant(300_000),
        )
        .unwrap();
    // Observed at 15 ms with 10 ms of clock uncertainty: 15 + 610 - 10 ms.
    assert_eq!(
        input.view_deadline(ClientInstant(300_000)).unwrap(),
        ClientInstant(615_000)
    );
    assert!(input.tick(ClientInstant(614_999)).unwrap());
    assert_eq!(input.tick(ClientInstant(615_000)), Ok(false));
    assert_eq!(input.suspended_since(), Some(ClientInstant(615_000)));
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
/// Deliver an authenticated ticket for `c` issued at host time `issued`.
fn deliver_ticket(client: &mut InputClient, id: u128, sequence: u64, issued: u64, now: u64) {
    let mut bytes = [0; INPUT_TICKET_BYTES];
    let n = input_ticket::encode(
        Ticket {
            credentials: InputCredentials {
                ticket: InputTicketId::from_raw(id),
                ..credentials()
            },
            sequence,
            issued_at_us: issued,
            expires_at_us: issued + 1_000_000,
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    client
        .accept_ticket(&bytes[..n], clock(), ClientInstant(now))
        .unwrap();
}
fn fresh(client: &mut InputClient, serial: u64, at: u64) {
    client
        .presented(
            PresentedObservation {
                session: credentials().session,
                serial,
                view: credentials().view,
                received_at: ClientInstant(at),
                source_age_upper_us: 5_000,
            },
            ClientInstant(at),
        )
        .unwrap();
}
#[test]
fn a_suspension_resumes_only_with_fresh_evidence_and_a_ticket_issued_after_the_hosts_lapse() {
    let mut client = input(credentials());
    client
        .confirm_mapping(
            credentials().session,
            credentials().view,
            ClientInstant(30_000),
        )
        .unwrap();
    fresh(&mut client, 1, 40_000);
    let mut out = [0; 512];
    assert_eq!(
        client
            .action(key(), &mut out, ClientInstant(50_000))
            .unwrap()
            .sequence,
        0
    );
    // The view (5 ms old at 40 ms, 250 ms bound) lapses at 285 ms: suspended.
    client.tick(ClientInstant(285_000)).unwrap();
    assert_eq!(client.suspended_since(), Some(ClientInstant(285_000)));
    // Fresh evidence alone does not resume...
    fresh(&mut client, 2, 300_000);
    assert_eq!(
        client.action(key(), &mut out, ClientInstant(301_000)),
        Err(input::Error::ViewSuspended)
    );
    // ...nor does a ticket the host may have issued before its own lapse.
    deliver_ticket(&mut client, 5, 1, 1_300_000, 320_000);
    assert_eq!(
        client.action(key(), &mut out, ClientInstant(321_000)),
        Err(input::Error::ViewSuspended)
    );
    // A ticket issued more than the 1 s ceiling after the suspension began,
    // with fresh evidence, resumes input on the same lease.
    fresh(&mut client, 3, 1_415_000);
    deliver_ticket(&mut client, 6, 2, 2_400_000, 1_420_000);
    assert_eq!(client.suspended_since(), None);
    let encoded = client
        .action(key(), &mut out, ClientInstant(1_421_000))
        .unwrap();
    assert_eq!(encoded.sequence, 1, "the refused actions consumed nothing");
    assert_eq!(
        client.suspension_totals(ClientInstant(1_421_000)),
        (1, 1_420_000 - 285_000)
    );
    assert_eq!(client.stopped(), None);
}
#[test]
fn a_suspension_that_outlasts_its_limit_ends_the_grant_as_a_stale_view() {
    let mut client = input(credentials());
    fresh(&mut client, 1, 40_000);
    client.tick(ClientInstant(285_000)).unwrap();
    let limit = 285_000 + input::MAX_VIEW_SUSPENSION_US;
    client.tick(ClientInstant(limit - 1)).unwrap();
    assert_eq!(
        client.tick(ClientInstant(limit)),
        Err(input::Error::Stopped(input::StopReason::ViewStale))
    );
}
#[test]
fn a_key_released_by_the_suspension_is_dropped_but_a_real_misuse_is_not() {
    let key = |usage, transition| Action::Key {
        key: PhysicalKey::new(usage).unwrap(),
        transition,
    };
    let mut client = input(credentials());
    client
        .confirm_mapping(
            credentials().session,
            credentials().view,
            ClientInstant(30_000),
        )
        .unwrap();
    fresh(&mut client, 1, 40_000);
    let mut out = [0; 512];
    let press = client
        .action(
            key(4, KeyTransition::Press),
            &mut out,
            ClientInstant(50_000),
        )
        .unwrap();
    assert_eq!(press.sequence, 0);
    // Suspended while key 4 is held; the host releases it at its own lapse.
    client.tick(ClientInstant(285_000)).unwrap();
    fresh(&mut client, 2, 1_415_000);
    deliver_ticket(&mut client, 6, 1, 2_400_000, 1_420_000);
    assert_eq!(client.suspended_since(), None);
    // The user's own release of that key, after resuming, is dropped.
    assert_eq!(
        client.action(
            key(4, KeyTransition::Release),
            &mut out,
            ClientInstant(1_421_000)
        ),
        Err(input::Error::ReleasedBySuspension)
    );
    // A genuinely unpaired release is still a misuse.
    assert_eq!(
        client.action(
            key(5, KeyTransition::Release),
            &mut out,
            ClientInstant(1_421_001)
        ),
        Err(input::Error::InvalidTransition)
    );
    // A new press and release of key 4 are ordinary input again.
    let press = client
        .action(
            key(4, KeyTransition::Press),
            &mut out,
            ClientInstant(1_421_002),
        )
        .unwrap();
    assert_eq!(press.sequence, 1);
}
