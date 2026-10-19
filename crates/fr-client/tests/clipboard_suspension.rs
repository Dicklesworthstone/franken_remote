//! The clipboard keeps its original grant while a stale view pauses it. These
//! policy tests use real grant/ticket codecs and explicit native sink fixtures.
#[path = "clipboard_controller/mod.rs"]
pub mod support;

use fr_client::{
    clipboard::Error,
    input::{Action, Unsent},
};
use fr_core::input::{KeyTransition, PhysicalKey};
use fr_wire::clipboard::session::{Offer, Pump};
use support::*;

#[test]
fn suspension_outlives_the_last_proof_but_only_a_fresh_ticket_and_view_resume_it() {
    let (mut input, mut lane) = attached();
    let network = lane.transport();
    input.suspend(client(500_000));
    assert_eq!(network.sample(client(500_000)), Err(Error::NotReady));
    assert_eq!(
        lane.offer(1, "not admitted while suspended", None, client(500_000)),
        Err(Error::NotReady)
    );
    assert!(!lane.is_closed());

    // The host may have renewed the same lease, but no ticket is issued during
    // the view lapse. Expiry of the last proof grants no clipboard authority.
    input.tick(client(3_100_000)).unwrap();
    assert_eq!(network.sample(client(3_100_000)), Err(Error::NotReady));
    assert_eq!(
        network.sample_maintenance(client(3_100_000)).unwrap(),
        host_at(3_100_000)
    );
    ready(&mut input, 1, 3_200_000);
    assert_eq!(network.sample(client(3_200_000)), Err(Error::NotReady));
    input
        .accept_ticket(
            &ticket(1, H + 3_200_000, H + 4_200_000),
            correlation(3_200_000),
            client(3_200_000),
        )
        .unwrap();
    assert_eq!(input.suspended_since(), None);
    network.sample(client(3_200_000)).unwrap();
    assert!(matches!(
        lane.offer(2, "a new copy on the same lane", None, client(3_200_000)),
        Ok(Offer::Queued(_))
    ));
    let mut gate = Gate::default();
    assert_eq!(
        lane.pump(&mut scratch(), &mut gate, || client(3_200_000)),
        Ok(Pump::RecordAccepted)
    );
    assert_eq!(gate.calls, 1);
    assert!(!lane.is_closed());
    assert_eq!(
        input
            .attach_clipboard(78, true, client(3_200_000))
            .unwrap_err(),
        Error::AlreadyAttached
    );
}

#[test]
fn a_post_lapse_ticket_can_arrive_before_the_view_that_resumes_the_clipboard() {
    let (mut input, mut lane) = attached();
    input.suspend(client(500_000));
    input
        .accept_ticket(
            &ticket(1, H + 2_000_000, H + 3_000_000),
            correlation(2_000_000),
            client(2_000_000),
        )
        .unwrap();
    assert_eq!(
        lane.offer(1, "no visible view yet", None, client(2_000_000)),
        Err(Error::NotReady)
    );
    ready(&mut input, 1, 2_050_000);
    assert_eq!(input.suspended_since(), None);
    assert!(matches!(
        lane.offer(2, "now both gates passed", None, client(2_050_000)),
        Ok(Offer::Queued(_))
    ));
}

#[test]
fn a_ticket_received_before_the_host_lapse_cannot_resume_the_clipboard() {
    let (mut input, mut lane) = attached();
    input.suspend(client(500_000));
    ready(&mut input, 1, 600_000);
    input
        .accept_ticket(
            &ticket(1, H + 400_000, H + 1_400_000),
            correlation(600_000),
            client(600_000),
        )
        .unwrap();
    assert!(input.suspended_since().is_some());
    assert_eq!(
        lane.offer(1, "old proof", None, client(600_000)),
        Err(Error::NotReady)
    );
    assert!(!lane.is_closed());
}

#[test]
fn original_item_deadlines_keep_running_while_the_view_is_suspended() {
    let (mut input, mut lane) = attached();
    lane.offer(1, "expires while paused", None, client(0))
        .unwrap();
    let mut gate = Gate::default();
    assert_eq!(
        lane.pump(&mut scratch(), &mut gate, || client(0)),
        Ok(Pump::RecordAccepted)
    );
    input.suspend(client(500_000));
    assert_eq!(
        lane.pump(&mut scratch(), &mut gate, || client(3_100_000)),
        Ok(Pump::Suspended)
    );
    assert_eq!(gate.calls, 1);
    assert_eq!(lane.retained_bytes(), 0);
    assert!(!lane.is_closed());
    ready(&mut input, 1, 3_200_000);
    input
        .accept_ticket(
            &ticket(1, H + 3_200_000, H + 4_200_000),
            correlation(3_200_000),
            client(3_200_000),
        )
        .unwrap();
    assert!(matches!(
        lane.pump(&mut scratch(), &mut gate, || client(3_200_000)),
        Ok(Pump::CancelAccepted(_))
    ));
    assert_eq!(lane.retained_bytes(), 0);
}

#[test]
fn silent_view_lapse_pauses_without_an_owner_tick_and_has_one_fixed_end() {
    let (mut input, mut lane) = attached();
    let network = lane.transport();
    assert_eq!(network.sample(client(1_500_000)), Err(Error::NotReady));
    assert_eq!(
        lane.offer(1, "not admitted", None, client(1_500_001)),
        Err(Error::NotReady)
    );
    assert_eq!(network.sample(client(3_000_000)), Err(Error::NotReady));
    // A newer authenticated clock remains usable beyond the first correlation.
    // Neither it nor this late owner tick moves the first observed lapse.
    input
        .accept_ticket(
            &ticket(1, H + 6_000_000, H + 7_000_000),
            correlation(6_000_000),
            client(6_000_000),
        )
        .unwrap();
    for elapsed in [8_000_000, 11_499_999] {
        assert_eq!(network.sample(client(elapsed)), Err(Error::NotReady));
        assert!(network.sample_maintenance(client(elapsed)).is_ok());
    }
    assert_eq!(network.sample(client(11_500_000)), Err(Error::Expired));
    assert!(
        lane.offer(2, "cannot revive", None, client(11_500_001))
            .is_err()
    );
    assert!(lane.is_closed());
}

#[test]
fn paused_maintenance_still_fences_clock_regression_and_owner_destruction() {
    let (mut input, mut lane) = attached();
    let network = lane.transport();
    input.suspend(client(500_000));
    network.sample_maintenance(client(500_001)).unwrap();
    assert_eq!(
        network.sample_maintenance(client(500_000)),
        Err(Error::Clock)
    );
    assert!(lane.offer(1, "fenced", None, client(500_002)).is_err());

    let (mut input, lane) = attached();
    let network = lane.transport();
    input.suspend(client(500_000));
    drop(input);
    assert_eq!(
        network.sample_maintenance(client(500_001)),
        Err(Error::Stopped)
    );
}

#[test]
fn a_completed_attachment_can_join_paused_without_granting_payload_permission() {
    let mut input = accepted();
    ready(&mut input, 0, 0);
    input.suspend(client(500_000));
    let mut lane = input.attach_clipboard(77, true, client(500_000)).unwrap();
    lane.local_switch().set_enabled(true);
    lane.peer_switch().set_enabled(true);
    assert!(!lane.is_closed());
    assert_eq!(
        lane.offer(1, "not ready", None, client(500_000)),
        Err(Error::NotReady)
    );
    assert_eq!(
        lane.pump(&mut scratch(), &mut Gate::default(), || client(500_000)),
        Ok(Pump::Suspended)
    );
    ready(&mut input, 1, 2_000_000);
    input
        .accept_ticket(
            &ticket(1, H + 2_000_000, H + 3_000_000),
            correlation(2_000_000),
            client(2_000_000),
        )
        .unwrap();
    assert!(matches!(
        lane.offer(2, "ready on the one attached lane", None, client(2_000_000)),
        Ok(Offer::Queued(_))
    ));
}

#[test]
fn a_different_lease_cannot_resume_the_original_clipboard() {
    let (mut input, mut lane) = attached();
    input.suspend(client(500_000));
    let mut foreign = grant().initial_ticket();
    foreign.sequence = 1;
    foreign.credentials.ticket = fr_core::ids::InputTicketId::from_raw(99);
    foreign.credentials.lease = fr_core::ids::InputLeaseId::from_raw(99);
    foreign.issued_at_us = H + 500_000;
    foreign.expires_at_us = H + 1_500_000;
    let mut bytes = [0; fr_wire::input_ticket::INPUT_TICKET_BYTES];
    fr_wire::input_ticket::encode(
        foreign,
        &mut bytes,
        &L,
        9,
        fr_wire::input::InputDirection::HostToViewer,
        fr_wire::input::InputDelivery::Reliable,
    )
    .unwrap();
    assert!(
        input
            .accept_ticket(&bytes, correlation(500_000), client(500_000))
            .is_err()
    );
    assert!(lane.offer(1, "wrong lease", None, client(500_000)).is_err());
    assert!(lane.is_closed());
}

#[test]
fn a_ticket_proof_that_expires_before_visibility_does_not_end_the_paused_lane() {
    let (mut input, mut lane) = attached();
    input.suspend(client(500_000));
    input
        .accept_ticket(
            &ticket(1, H + 2_000_000, H + 3_000_000),
            correlation(2_000_000),
            client(2_000_000),
        )
        .unwrap();
    // The qualified ticket arrived first, but its lease proof has lapsed by
    // the time a picture becomes visible. Keep waiting for a live proof.
    ready(&mut input, 1, 3_200_000);
    assert_eq!(
        lane.offer(1, "proof is still lapsed", None, client(3_200_000)),
        Err(Error::NotReady)
    );
    assert!(!lane.is_closed());
    input
        .accept_ticket(
            &ticket(2, H + 3_200_000, H + 4_200_000),
            correlation(3_200_000),
            client(3_200_000),
        )
        .unwrap();
    assert!(matches!(
        lane.offer(2, "current proof and visible view", None, client(3_200_000)),
        Ok(Offer::Queued(_))
    ));
    assert!(!lane.is_closed());
}

#[test]
fn a_late_first_owner_tick_preserves_the_original_view_lapse_deadline() {
    let (mut input, mut lane) = attached();
    let network = lane.transport();
    // No worker or owner polls at the 1.5 s view deadline. Discovering that
    // lapse much later must not start a new ten-second suspension allowance.
    input.tick(client(6_000_000)).unwrap();
    input
        .accept_ticket(
            &ticket(1, H + 6_000_000, H + 7_000_000),
            correlation(6_000_000),
            client(6_000_000),
        )
        .unwrap();
    assert_eq!(network.sample(client(11_499_999)), Err(Error::NotReady));
    assert_eq!(
        network.sample_maintenance(client(11_500_000)),
        Err(Error::Expired)
    );
    assert!(
        lane.offer(1, "bound already ended", None, client(11_500_001))
            .is_err()
    );
    assert!(lane.is_closed());
}

#[test]
fn closing_a_paused_lane_is_terminal_for_both_transport_clock_apis() {
    let (mut input, mut lane) = attached();
    let network = lane.transport();
    input.suspend(client(500_000));
    assert_eq!(network.sample(client(500_000)), Err(Error::NotReady));
    assert!(network.sample_maintenance(client(500_000)).is_ok());
    lane.close();
    assert_eq!(network.sample(client(500_000)), Err(Error::Stopped));
    assert_eq!(
        network.sample_maintenance(client(500_000)),
        Err(Error::Stopped)
    );
}

#[test]
fn abandoning_a_never_admitted_action_removes_its_clipboard_receipt_obligation() {
    let (mut input, mut lane) = attached();
    let network = lane.transport();
    let encoded = input
        .action(
            Action::Key {
                key: PhysicalKey::new(4).unwrap(),
                transition: KeyTransition::Press,
            },
            &mut [0; 256],
            client(100_000),
        )
        .unwrap();
    input.suspend(client(200_000));
    // The caller's unsent slot was never admitted to transport. Suspension
    // consumes it locally; there can be no host receipt for these bytes.
    input
        .abandon_unsent(Unsent::Action(encoded.sequence))
        .unwrap();
    assert_eq!(input.pending_actions(), 0);
    let old_receipt_deadline = 100_000 + policy().receipt_timeout_us;
    // A detached worker checks the old deadline before any new input event
    // can refresh the projection. Only the view gate should still block it.
    assert_eq!(
        network
            .sample_maintenance(client(old_receipt_deadline))
            .unwrap(),
        host_at(old_receipt_deadline)
    );
    assert_eq!(
        network.sample(client(old_receipt_deadline)),
        Err(Error::NotReady)
    );
    input.tick(client(old_receipt_deadline)).unwrap();

    ready(&mut input, 1, 1_600_000);
    input
        .accept_ticket(
            &ticket(1, H + 1_600_000, H + 2_600_000),
            correlation(1_600_000),
            client(1_600_000),
        )
        .unwrap();
    assert_eq!(input.suspended_since(), None);
    assert!(matches!(
        lane.offer(
            1,
            "same clipboard after unsent action",
            None,
            client(1_600_000)
        ),
        Ok(Offer::Queued(_))
    ));
    assert!(!lane.is_closed());
}
