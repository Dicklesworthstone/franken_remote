//! Actual grant, input authority, ticket codecs and clipboard owners under
//! explicit clocks. Recording platform/transport sinks are not a WAN benchmark.
#[path = "clipboard_controller/mod.rs"]
pub mod support;

use fr_client::{
    clipboard::Error,
    input::{Action, Error as InputError, InputClient, StopReason},
};
use fr_core::{
    clipboard::Publication,
    ids::InputTicketId,
    input::{KeyTransition, PhysicalKey},
    input_submission::InputSession,
    time::HostInstant,
};
use fr_wire::{
    clipboard::session::{Admission, Offer, Pump, RecordSink, SessionError, TransportFailure},
    clipboard::{self, Body, CancelReason, Role, session::egress::Egress},
    input::{InputDelivery as T, InputDirection as D},
    input_ticket::{self, AuthorityTicket, Ticket},
};
use support::*;

fn encode(record: AuthorityTicket) -> Vec<u8> {
    let mut bytes = vec![0; input_ticket::AUTHORITY_TICKET_BYTES];
    input_ticket::encode_authority(record, &mut bytes, &L, 9, D::HostToViewer, T::Reliable)
        .unwrap();
    bytes
}
fn proof(sequence: u64, issued: u64, ticket_until: u64, authority_until: u64) -> Vec<u8> {
    let mut ticket = grant().initial_ticket();
    ticket.sequence = sequence;
    ticket.credentials.ticket = InputTicketId::from_raw(100 + u128::from(sequence));
    ticket.issued_at_us = issued;
    ticket.expires_at_us = ticket_until;
    encode(AuthorityTicket {
        ticket,
        authority_until_us: authority_until,
    })
}
fn renew(host: &mut InputSession, input: &mut InputClient, sequence: u64, elapsed: u64) {
    let at = host_at(elapsed);
    let nonce = u128::from(sequence) * 2;
    host.issue_observation_challenge(nonce, at).unwrap();
    host.renew_observation(nonce, at).unwrap();
    host.issue_control_challenge(nonce + 1, at).unwrap();
    host.renew_control(nonce + 1, at).unwrap();
    let id = InputTicketId::from_raw(100 + u128::from(sequence));
    let (issued, expires, authority) = host.issue_ticket_authority_timed(id, at).unwrap();
    let bytes = encode(AuthorityTicket {
        ticket: Ticket {
            credentials: host.ticket_credentials(id),
            sequence,
            issued_at_us: issued.as_micros(),
            expires_at_us: expires.as_micros(),
        },
        authority_until_us: authority.as_micros(),
    });
    input
        .accept_authority_ticket(&bytes, correlation(elapsed), client(elapsed))
        .unwrap();
    ready(input, sequence + 2, elapsed);
}

#[derive(Default)]
struct Retain(Option<Egress>);
impl RecordSink for Retain {
    fn try_send(&mut self, _: &[u8]) -> Result<Admission, TransportFailure> {
        panic!("the original permit must cross the handoff")
    }
    fn try_send_checked(
        &mut self,
        _: &[u8],
        permit: Egress,
    ) -> Result<Admission, TransportFailure> {
        self.0 = Some(permit);
        Ok(Admission::Accepted)
    }
}

#[test]
fn one_mib_crosses_both_directions_past_the_short_ticket_bound_on_original_owners() {
    for from_viewer in [false, true] {
        let (mut input, mut viewer) = attached();
        let mut host_input = host_owner();
        let mut host = host_channel(&host_input);
        ready(&mut input, 1, 1_000_000);
        renew(&mut host_input, &mut input, 1, 2_000_000);
        let initial_ticket_until = input.ticket_deadline().unwrap();
        let text = "🦀".repeat(262_144);
        let offered = if from_viewer {
            viewer.offer(1, &text, None, client(2_000_000)).unwrap()
        } else {
            host.offer(1, &text, None, host_at(2_000_000)).unwrap()
        };
        assert!(matches!(offered, Offer::Queued(_)));
        let mut platform = Platform::default();
        let mut gate = Gate::default();
        let mut completed = None;
        for record in 0..70 {
            let elapsed = 2_000_000 + record * 30_000;
            if record > 0 && record % 8 == 0 {
                renew(&mut host_input, &mut input, 1 + record / 8, elapsed);
            }
            let mut bytes = scratch();
            let progress = if from_viewer {
                viewer
                    .pump(&mut bytes, &mut gate, || client(elapsed))
                    .unwrap()
            } else {
                host.pump(&mut bytes, &mut gate, || host_at(elapsed))
                    .unwrap()
            };
            assert!(bytes.iter().all(|b| *b == 0));
            let receipt = if from_viewer {
                host.receive(&gate.last, &mut platform, || host_at(elapsed))
                    .unwrap()
            } else {
                viewer
                    .receive(&gate.last, &mut platform, || client(elapsed))
                    .unwrap()
            };
            if matches!(progress, Pump::ItemAccepted(_)) {
                assert_eq!(receipt.unwrap().publication, Publication::SubmittedToOs);
                completed = Some(client(elapsed));
                break;
            }
            assert_eq!(receipt, None);
            assert_eq!(platform.text, Vec::<String>::new());
        }
        assert!(completed.unwrap() > initial_ticket_until);
        assert_eq!(gate.calls, 66);
        assert_eq!(platform.text, [text]);
        assert!(!viewer.is_closed());
        assert_eq!(input.stopped(), None);
    }
}

#[test]
fn later_authority_proof_cannot_extend_an_original_item_or_its_retained_handoff() {
    let (mut input, mut lane) = attached();
    let network = lane.transport();
    lane.offer(1, "original item", None, client(0)).unwrap();
    let mut retained = Retain::default();
    lane.pump(&mut scratch(), &mut retained, || client(0))
        .unwrap();
    let old = retained.0.take().unwrap();
    let original = HostInstant::from_micros(H + 3_000_000);
    assert_eq!(old.deadline(), original);
    ready(&mut input, 1, 1_000_000);
    renew(&mut host_owner(), &mut input, 1, 2_000_000);
    assert_eq!(old.deadline(), original);
    let at = network.sample(client(2_999_900)).unwrap();
    assert_eq!(at, original);
    assert_eq!(
        old.check_operation(at),
        Err(SessionError::Clipboard(fr_core::clipboard::Error::Expired))
    );
    let mut gate = Gate::default();
    assert!(matches!(
        lane.pump(&mut scratch(), &mut gate, || client(2_999_900))
            .unwrap(),
        Pump::CancelAccepted(_)
    ));
    assert!(matches!(
        clipboard::decode(&gate.last, context(Role::Controller), &L)
            .unwrap()
            .body,
        Body::Cancel(CancelReason::Expired)
    ));
    assert_eq!(lane.retained_bytes(), 0);
    lane.offer(2, "new item", None, client(2_999_901)).unwrap();
    lane.pump(&mut scratch(), &mut retained, || client(2_999_901))
        .unwrap();
    assert!(retained.0.unwrap().deadline() > original);
    assert!(!lane.is_closed());
}

#[test]
fn a_shorter_actual_cap_fences_both_auxiliary_owners_without_retiming_old_permits() {
    let (mut input, mut lane) = attached();
    let files = input
        .file_receive_authority(grant().request.parent, 78, grant().lease, client(0))
        .unwrap();
    let network = lane.transport();
    lane.offer(1, "old cap", None, client(0)).unwrap();
    let mut retained = Retain::default();
    lane.pump(&mut scratch(), &mut retained, || client(0))
        .unwrap();
    let old = retained.0.take().unwrap();
    input
        .accept_authority_ticket(
            &proof(1, H + 100_000, H + 600_000, H + 1_200_000),
            correlation(100_000),
            client(100_000),
        )
        .unwrap();
    let at = files.sample(client(1_000_000)).unwrap();
    assert_eq!(
        files.deadline(at).unwrap(),
        HostInstant::from_micros(H + 1_200_000)
    );
    // The network owner checks its independent authority cursor before the
    // retained operation bound, just as the production async handoff does.
    old.check_operation(network.sample(client(1_000_000)).unwrap())
        .unwrap();
    assert_eq!(old.deadline(), HostInstant::from_micros(H + 3_000_000));
    assert_eq!(network.sample(client(1_200_000)), Err(Error::Expired));
    assert!(files.sample(client(1_200_000)).is_err());
    assert!(
        lane.pump(&mut scratch(), &mut Gate::default(), || client(1_200_000))
            .is_err()
    );
    assert_eq!(
        old.check_operation(host_at(1_200_000)),
        Err(SessionError::Clipboard(fr_core::clipboard::Error::Closed))
    );
}

#[test]
fn a_longer_authority_cap_never_extends_the_action_ticket() {
    let (mut input, mut lane) = attached();
    input
        .accept_authority_ticket(
            &proof(1, H + 100_000, H + 600_000, H + 3_000_000),
            correlation(100_000),
            client(100_000),
        )
        .unwrap();
    assert!(input.ticket_deadline().unwrap() < client(600_000));
    let action = Action::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: KeyTransition::Press,
    };
    assert_eq!(
        input.action(
            action,
            &mut [0; fr_wire::input::MAX_INPUT_RECORD_BYTES],
            client(600_000)
        ),
        Err(InputError::TicketExpired)
    );
    assert_eq!(input.pending_actions(), 0);
    assert!(
        lane.offer(1, "independently bounded clipboard", None, client(600_000))
            .is_ok()
    );
}

#[test]
fn an_expired_short_ticket_cannot_install_its_still_future_authority_cap() {
    let (mut input, mut lane) = attached();
    ready(&mut input, 1, 1_000_000);
    ready(&mut input, 2, 2_000_000);
    assert_eq!(
        input.accept_authority_ticket(
            &proof(1, H + 1_000_000, H + 1_500_000, H + 4_000_000),
            correlation(2_000_000),
            client(2_000_000),
        ),
        Err(InputError::TicketExpired)
    );
    assert_eq!(
        lane.offer(1, "expired old proof", None, client(3_000_000)),
        Err(Error::Expired)
    );
    assert_eq!(input.stopped(), None);
}

#[test]
fn an_obsolete_generation_ticket_consumes_sequence_without_refreshing_auxiliary_authority() {
    let (mut input, lane) = attached();
    let files = input
        .file_receive_authority(grant().request.parent, 78, grant().lease, client(0))
        .unwrap();
    input.suspend(client(100_000));
    input
        .advance_recovery(
            grant().credentials().view.recovery.next().unwrap(),
            client(100_000),
        )
        .unwrap();
    let old = proof(1, H + 200_000, H + 1_200_000, H + 3_200_000);
    input
        .accept_authority_ticket(&old, correlation(200_000), client(200_000))
        .unwrap();
    assert_eq!(input.ticket_deadline(), None);
    assert_eq!(input.suspended_since(), Some(client(100_000)));
    // File authority never borrows the clipboard's bounded pause exception.
    assert!(files.sample(client(3_000_000)).is_err());
    assert!(!lane.is_closed());
    assert_eq!(
        input.accept_authority_ticket(&old, correlation(3_000_000), client(3_000_000)),
        Err(InputError::Stopped(StopReason::InvalidTicket))
    );
}

#[test]
fn authority_evidence_does_not_substitute_for_a_post_lapse_ticket_or_visible_view() {
    let (mut input, mut lane) = attached();
    input.suspend(client(500_000));
    ready(&mut input, 1, 600_000);
    input
        .accept_authority_ticket(
            &proof(1, H + 400_000, H + 1_400_000, H + 3_400_000),
            correlation(600_000),
            client(600_000),
        )
        .unwrap();
    assert!(input.suspended_since().is_some());
    assert_eq!(
        lane.offer(1, "pre-lapse proof", None, client(600_000)),
        Err(Error::NotReady)
    );
    input
        .accept_authority_ticket(
            &proof(2, H + 2_200_000, H + 3_200_000, H + 5_200_000),
            correlation(2_200_000),
            client(2_200_000),
        )
        .unwrap();
    assert!(input.suspended_since().is_some());
    assert_eq!(
        lane.offer(2, "no current presentation", None, client(2_200_000)),
        Err(Error::NotReady)
    );
    ready(&mut input, 2, 2_200_001);
    assert!(input.suspended_since().is_none());
    assert!(
        lane.offer(3, "same owner resumed", None, client(2_200_001))
            .is_ok()
    );
}

#[test]
fn wrong_profile_foreign_scope_and_replay_stop_the_original_owner() {
    for offset in [
        None,
        Some(24),
        Some(40),
        Some(72),
        Some(80),
        Some(88),
        Some(96),
    ] {
        let (mut input, mut lane) = attached();
        let mut bytes = proof(1, H + 1000, H + 1_000_000, H + 3_000_000);
        if let Some(offset) = offset {
            bytes[offset] ^= 1;
        } else {
            bytes = ticket(1, H + 1000, H + 1_000_000);
        }
        assert!(
            input
                .accept_authority_ticket(&bytes, correlation(1000), client(1000))
                .is_err()
        );
        assert_eq!(input.stopped(), Some(StopReason::InvalidTicket));
        assert!(lane.offer(1, "cannot revive", None, client(1001)).is_err());
    }
    let (mut input, _) = attached();
    let bytes = proof(1, H + 1000, H + 1_000_000, H + 3_000_000);
    input
        .accept_authority_ticket(&bytes, correlation(1000), client(1000))
        .unwrap();
    assert_eq!(
        input.accept_authority_ticket(&bytes, correlation(1001), client(1001)),
        Err(InputError::Stopped(StopReason::InvalidTicket))
    );
    let (mut legacy, _) = attached();
    assert!(
        legacy
            .accept_ticket(&bytes, correlation(1000), client(1000))
            .is_err()
    );
    assert_eq!(legacy.stopped(), Some(StopReason::InvalidTicket));
}

#[test]
fn authority_tickets_cannot_create_a_grant_or_resurrect_a_dropped_owner() {
    let g = grant();
    let mut ungranted = InputClient::new(
        g.credentials(),
        9,
        g.request.target.bounds,
        g.request.target.capabilities,
        L,
        policy(),
        client(0),
    )
    .unwrap();
    ready(&mut ungranted, 0, 0);
    let bytes = proof(1, H + 1000, H + 1_000_000, H + 3_000_000);
    ungranted
        .accept_authority_ticket(&bytes, correlation(1000), client(1000))
        .unwrap();
    assert_eq!(
        ungranted
            .attach_clipboard(77, true, client(1000))
            .unwrap_err(),
        Error::NotGranted
    );
    let (input, mut old) = attached();
    old.offer(1, "old lifetime", None, client(0)).unwrap();
    drop(input);
    let (mut replacement, mut new) = attached();
    replacement
        .accept_authority_ticket(&bytes, correlation(1000), client(1000))
        .unwrap();
    assert!(new.offer(2, "new lifetime", None, client(1000)).is_ok());
    assert!(old.offer(3, "must stay ended", None, client(1000)).is_err());
    replacement.stop(StopReason::FocusLost);
    assert_eq!(
        replacement.accept_authority_ticket(
            &proof(2, H + 2000, H + 1_002_000, H + 3_002_000),
            correlation(2000),
            client(2000)
        ),
        Err(InputError::Stopped(StopReason::FocusLost))
    );
    assert!(new.offer(4, "terminal", None, client(2000)).is_err());
}
