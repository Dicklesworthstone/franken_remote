//! Actual grant/ticket codecs, measured clock intervals and reporter admission
//! receipts under explicit clocks. Renderer visibility and transport admission
//! are test boundaries here; this does not qualify a native presentation path.
#[path = "clipboard_controller/mod.rs"]
pub mod support;

use fr_client::input::{
    Action, Error, InputClient, MAX_VIEW_SUSPENSION_US, PresentedObservation, StopReason,
};
use fr_core::{
    ids::{InputTicketId, RecoveryGeneration},
    input::{InputRequest, KeyTransition, PhysicalKey},
};
use fr_media::presented::{REPORT_INTERVAL_US, Reporter};
use fr_wire::{
    SourceObservation,
    decoder::Binding,
    input::{self, InputDelivery as T, InputDirection as D, MAX_INPUT_RECORD_BYTES},
    input_ticket::{self, AuthorityTicket, ReadinessTicket},
    presented::{BASE_SOURCE_AGE_US, MAX_SOURCE_AGE_US, Sample, Stamp},
};
use support::{H, L, accepted, client, correlation, grant, host_at};

fn binding(recovery: RecoveryGeneration) -> Binding {
    let g = grant();
    let view = g.request.target.view;
    Binding {
        parent: g.request.parent,
        display: u128::from(g.request.target.display_binding),
        geometry: view.geometry,
        configuration: view.configuration,
        recovery,
        viewport: view.viewport,
    }
}

fn sample(elapsed: u64) -> Sample {
    let observed_us = H + elapsed;
    Sample {
        stamp: Stamp {
            frame: elapsed + 1,
            captured_us: observed_us,
            observed_us,
            source: SourceObservation::Captured,
        },
        age_upper_us: correlation(elapsed)
            .age_upper_us(observed_us, client(elapsed).0)
            .unwrap(),
    }
}

fn record(
    sequence: u64,
    report: u64,
    elapsed: u64,
    recovery: RecoveryGeneration,
) -> ReadinessTicket {
    let mut ticket = grant().initial_ticket();
    ticket.credentials.ticket = InputTicketId::from_raw(100 + u128::from(sequence));
    ticket.credentials.view.recovery = recovery;
    ticket.sequence = sequence;
    ticket.issued_at_us = host_at(elapsed).as_micros();
    ticket.expires_at_us = ticket.issued_at_us + 1_000_000;
    ReadinessTicket {
        authority: AuthorityTicket {
            ticket,
            authority_until_us: grant().lease_until_us,
        },
        presented_sequence: report,
    }
}

fn encode(record: ReadinessTicket) -> Vec<u8> {
    let mut bytes = vec![0; input_ticket::READINESS_TICKET_BYTES];
    input_ticket::encode_readiness(
        record,
        &mut bytes,
        &L,
        grant().input_channel,
        D::HostToViewer,
        T::Reliable,
    )
    .unwrap();
    bytes
}

fn press() -> Action<'static> {
    Action::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: KeyTransition::Press,
    }
}

fn decode(bytes: &[u8]) -> InputRequest<'_> {
    input::decode_input(
        bytes,
        &L,
        grant().input_channel,
        D::ViewerToHost,
        T::Reliable,
    )
    .unwrap()
}

struct Fixture {
    input: InputClient,
    reporter: Reporter,
    recovery: RecoveryGeneration,
    serial: u64,
}

impl Fixture {
    fn new() -> Self {
        let recovery = RecoveryGeneration::INITIAL;
        let mut reporter = Reporter::new(binding(recovery), L, client(0).0).unwrap();
        reporter
            .prepare(Some(sample(0)), client(0).0, BASE_SOURCE_AGE_US)
            .unwrap();
        // Observation already admitted this positive report before promotion.
        // Import its actual cursor instead of inventing an initial sequence.
        assert_eq!(reporter.queued_receipt(client(0).0).unwrap().sequence(), 1);
        let mut input = accepted();
        input.expect_readiness_feedback(client(0)).unwrap();
        input
            .bind_readiness_reporter(reporter.admitted_cursor(), client(0))
            .unwrap();
        input
            .confirm_mapping(
                grant().credentials().session,
                grant().credentials().view,
                client(0),
            )
            .unwrap();
        let mut fixture = Self {
            input,
            reporter,
            recovery,
            serial: 0,
        };
        fixture.show(0);
        fixture
    }

    fn show(&mut self, elapsed: u64) {
        let mut view = grant().credentials().view;
        view.recovery = self.recovery;
        self.input
            .presented(
                PresentedObservation {
                    session: grant().credentials().session,
                    serial: self.serial,
                    view,
                    received_at: client(elapsed),
                    source_age_upper_us: sample(elapsed).age_upper_us,
                },
                client(elapsed),
            )
            .unwrap();
        self.serial += 1;
    }

    fn negative(&mut self, elapsed: u64) -> u64 {
        let reserved = self.reporter.force_unavailable(client(elapsed).0).unwrap();
        let receipt = self.reporter.queued_receipt(client(elapsed).0).unwrap();
        assert_eq!(receipt.sequence(), reserved);
        assert_eq!(receipt.visible(), None);
        self.input
            .report_admitted(receipt, client(elapsed))
            .unwrap();
        reserved
    }

    fn positive(&mut self, elapsed: u64) -> u64 {
        let visible = sample(elapsed);
        self.reporter
            .prepare(Some(visible), client(elapsed).0, BASE_SOURCE_AGE_US)
            .unwrap();
        let receipt = self.reporter.queued_receipt(client(elapsed).0).unwrap();
        let sequence = receipt.sequence();
        assert_eq!(receipt.visible(), Some(visible.stamp));
        self.input
            .report_admitted(receipt, client(elapsed))
            .unwrap();
        sequence
    }

    fn ack(&mut self, sequence: u64, report: u64, elapsed: u64) -> Result<(), Error> {
        self.input.accept_readiness_ticket(
            &encode(record(sequence, report, elapsed, self.recovery)),
            correlation(elapsed),
            client(elapsed),
        )
    }

    fn blocked(&mut self, elapsed: u64) {
        let pending = self.input.pending_actions();
        assert_eq!(
            self.input
                .action(press(), &mut [0; MAX_INPUT_RECORD_BYTES], client(elapsed)),
            Err(Error::ViewSuspended)
        );
        assert_eq!(self.input.pending_actions(), pending);
        assert_eq!(self.input.stopped(), None);
    }
}

#[test]
fn a_causal_ack_resumes_the_same_generation_before_the_legacy_age_wait() {
    let mut f = Fixture::new();
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    assert_eq!(
        f.input
            .action(press(), &mut out, client(1))
            .unwrap()
            .sequence,
        0
    );
    f.input.suspend(client(10_000));
    assert!(f.input.needs_loss_report());
    assert!(!f.input.holds_key(PhysicalKey::new(4).unwrap()));
    assert_eq!(f.negative(10_000), 2);
    f.show(20_000);
    assert_eq!(f.positive(60_000), 3);
    f.blocked(60_000);
    f.ack(1, 3, 60_100).unwrap();
    assert_eq!(f.input.suspended_since(), None);
    let totals = f.input.suspension_totals(client(60_100));
    assert_eq!(totals, (1, 50_100));
    assert!(totals.1 < MAX_SOURCE_AGE_US);
    assert_eq!(f.input.pending_actions(), 1);
    let next = f.input.action(press(), &mut out, client(60_101)).unwrap();
    let request = decode(&out[..next.bytes]);
    assert_eq!(next.sequence, 1);
    assert_eq!(request.credentials.session, grant().credentials().session);
    assert_eq!(request.credentials.lease, grant().lease);
    assert_eq!(request.credentials.view, grant().credentials().view);
    assert_eq!(request.credentials.ticket, InputTicketId::from_raw(101));
    assert_eq!(f.input.pending_actions(), 2);
}

#[test]
fn an_earlier_revival_ticket_cannot_unlock_a_second_lapse() {
    let mut f = Fixture::new();
    f.input.suspend(client(10_000));
    assert_eq!(f.negative(10_000), 2);
    f.show(60_000);
    assert_eq!(f.positive(60_000), 3);
    f.ack(1, 3, 60_100).unwrap();
    let delayed = encode(record(2, 3, 60_101, f.recovery));
    f.input.suspend(client(70_000));
    assert_eq!(f.negative(70_000), 4);
    f.show(120_000);
    assert_eq!(f.positive(120_000), 5);
    f.input
        .accept_readiness_ticket(&delayed, correlation(120_100), client(120_100))
        .unwrap();
    assert_eq!(f.input.suspended_since(), Some(client(70_000)));
    f.blocked(120_100);
    f.ack(3, 5, 120_101).unwrap();
    assert_eq!(f.input.suspended_since(), None);
    assert_eq!(f.input.suspension_totals(client(120_101)), (2, 100_201));
}

#[test]
fn a_prepared_or_backpressured_negative_cannot_establish_the_loss_barrier() {
    let mut f = Fixture::new();
    f.input.suspend(client(10_000));
    let original_cursor = f.reporter.admitted_cursor();
    assert_eq!(f.reporter.force_unavailable(client(10_000).0), Ok(2));
    let (bytes, until) = f.reporter.pending(client(10_000).0).unwrap().unwrap();
    let original = (bytes.to_vec(), until);
    assert_eq!(until, client(10_000).0 + REPORT_INTERVAL_US);
    f.show(20_000);
    f.reporter
        .prepare(Some(sample(20_000)), client(20_000).0, BASE_SOURCE_AGE_US)
        .unwrap();
    // A blocked send never calls queued_receipt; both bytes and send-by remain.
    let (bytes, until) = f.reporter.pending(client(20_000).0).unwrap().unwrap();
    assert_eq!((bytes.to_vec(), until), original);
    assert_eq!(f.reporter.admitted_cursor(), original_cursor);
    f.input
        .bind_readiness_reporter(original_cursor, client(20_000))
        .unwrap();
    f.ack(1, 1, 20_001).unwrap();
    assert!(f.input.needs_loss_report());
    f.blocked(20_001);
    let loss = f.reporter.queued_receipt(client(20_002).0).unwrap();
    f.input.report_admitted(loss, client(20_002)).unwrap();
    assert!(!f.input.needs_loss_report());
    f.show(70_002);
    assert_eq!(f.positive(70_002), 3);
    f.ack(2, 3, 70_003).unwrap();
    assert_eq!(f.input.suspended_since(), None);
}

#[test]
fn another_fresh_to_stale_transition_needs_a_barrier_but_keeps_the_original_timeout() {
    let mut f = Fixture::new();
    f.input.suspend(client(10_000));
    assert_eq!(f.negative(10_000), 2);
    f.show(20_000);
    assert_eq!(f.positive(60_000), 3);
    let delayed = encode(record(1, 3, 60_000, f.recovery));
    // The first suspension still awaits its ack when the picture lapses again.
    f.input.suspend(client(70_000));
    assert_eq!(f.input.suspended_since(), Some(client(10_000)));
    assert!(f.input.needs_loss_report());
    f.show(71_000);
    f.input
        .accept_readiness_ticket(&delayed, correlation(71_000), client(71_000))
        .unwrap();
    assert!(f.input.needs_loss_report());
    f.blocked(71_000);
    assert_eq!(f.negative(71_001), 4);
    f.show(121_001);
    assert_eq!(f.positive(121_001), 5);
    f.blocked(121_001);
    let original_until = 10_000 + MAX_VIEW_SUSPENSION_US;
    f.input.tick(client(original_until - 1)).unwrap();
    assert_eq!(f.input.suspended_since(), Some(client(10_000)));
    assert_eq!(
        f.input.tick(client(original_until)),
        Err(Error::Stopped(StopReason::ViewStale))
    );
    assert_eq!(
        f.input.suspension_totals(client(original_until)),
        (1, MAX_VIEW_SUSPENSION_US)
    );
}

#[test]
fn positive_admission_and_acknowledgement_still_require_current_local_visibility() {
    for show_before_ack in [false, true] {
        let mut f = Fixture::new();
        f.input.suspend(client(10_000));
        assert_eq!(f.negative(10_000), 2);
        assert_eq!(f.positive(60_000), 3);
        if show_before_ack {
            f.show(60_000);
        }
        f.blocked(60_000);
        f.ack(1, 3, 60_100).unwrap();
        if !show_before_ack {
            f.blocked(60_100);
            assert_eq!(f.input.suspended_since(), Some(client(10_000)));
            f.show(60_101);
        }
        assert_eq!(f.input.suspended_since(), None);
        assert_eq!(
            f.input
                .action(press(), &mut [0; MAX_INPUT_RECORD_BYTES], client(60_102))
                .unwrap()
                .sequence,
            0
        );
    }
}

#[test]
fn future_and_regressing_report_acknowledgements_stop_the_original_owner() {
    for wrong_report in [1, 3] {
        let mut f = Fixture::new();
        assert_eq!(f.positive(50_000), 2);
        f.show(50_000);
        f.ack(1, 2, 50_100).unwrap();
        assert_eq!(
            f.ack(2, wrong_report, 50_101),
            Err(Error::Stopped(StopReason::InvalidTicket))
        );
        assert_eq!(f.input.stopped(), Some(StopReason::InvalidTicket));
        assert_eq!(
            f.ack(3, 2, 50_102),
            Err(Error::Stopped(StopReason::InvalidTicket))
        );
        assert_eq!(f.input.pending_actions(), 0);
    }
}

#[test]
fn an_expired_ticket_consumes_replay_history_without_advancing_readiness() {
    for (replay, expired_ack) in [(false, 3), (false, 4), (true, 3), (true, 4)] {
        let mut f = Fixture::new();
        f.input.suspend(client(10_000));
        assert_eq!(f.negative(10_000), 2);
        f.show(60_000);
        assert_eq!(f.positive(60_000), 3);
        let mut expired = record(1, expired_ack, 60_000, f.recovery);
        expired.authority.ticket.expires_at_us = host_at(60_050).as_micros();
        // Ack 3 would resume this view; ack 4 exceeds its actual admitted
        // ceiling. Expiry must win before either can change readiness state.
        assert_eq!(
            f.input
                .accept_readiness_ticket(&encode(expired), correlation(60_100), client(60_100),),
            Err(Error::TicketExpired)
        );
        assert_eq!(f.input.suspended_since(), Some(client(10_000)));
        f.blocked(60_100);
        if replay {
            // Making that consumed ticket sequence timely cannot reuse it.
            assert_eq!(
                f.ack(1, 3, 60_101),
                Err(Error::Stopped(StopReason::InvalidTicket))
            );
        } else {
            // In particular, expired ack 4 did not become a high-water mark
            // that would reject this actual, still-current positive report.
            f.ack(2, 3, 60_101).unwrap();
            assert_eq!(f.input.suspended_since(), None);
            assert_eq!(
                f.input
                    .action(press(), &mut [0; MAX_INPUT_RECORD_BYTES], client(60_102))
                    .unwrap()
                    .sequence,
                0
            );
        }
    }
}

#[test]
fn a_foreign_reporter_cannot_substitute_numerically_identical_receipts_or_cursors() {
    for use_receipt in [false, true] {
        let mut f = Fixture::new();
        let mut foreign = Reporter::new(binding(f.recovery), L, client(0).0).unwrap();
        foreign
            .prepare(Some(sample(0)), client(0).0, BASE_SOURCE_AGE_US)
            .unwrap();
        foreign.queued_receipt(client(0).0).unwrap();
        let original = f.reporter.admitted_cursor();
        assert_eq!(foreign.admitted_cursor().binding(), original.binding());
        assert_eq!(foreign.admitted_cursor().sequence(), original.sequence());
        let result = if use_receipt {
            foreign.force_unavailable(client(10_000).0).unwrap();
            let receipt = foreign.queued_receipt(client(10_000).0).unwrap();
            assert_eq!(receipt.sequence(), original.sequence() + 1);
            f.input.report_admitted(receipt, client(10_000))
        } else {
            f.input
                .bind_readiness_reporter(foreign.admitted_cursor(), client(10_000))
        };
        assert_eq!(result, Err(Error::Stopped(StopReason::InvalidTicket)));
        assert_eq!(f.input.stopped(), Some(StopReason::InvalidTicket));
    }
}

#[test]
fn a_new_cursor_cannot_skip_unconsumed_receipts_from_the_original_reporter() {
    for negative in [false, true] {
        let mut f = Fixture::new();
        if negative {
            f.reporter.force_unavailable(client(50_000).0).unwrap();
        } else {
            f.reporter
                .prepare(Some(sample(50_000)), client(50_000).0, BASE_SOURCE_AGE_US)
                .unwrap();
        }
        let receipt = f.reporter.queued_receipt(client(50_000).0).unwrap();
        assert_eq!(receipt.sequence(), 2);
        // Admission happened, but the owner did not consume that exact receipt.
        // Rebinding a newer cursor cannot erase a missed negative transition.
        assert_eq!(
            f.input
                .bind_readiness_reporter(f.reporter.admitted_cursor(), client(50_000)),
            Err(Error::Stopped(StopReason::InvalidTicket))
        );
    }
}

#[test]
fn a_successor_reporter_cannot_advance_recovery_without_the_explicit_input_transition() {
    let mut f = Fixture::new();
    f.input.suspend(client(10_000));
    let next = f.recovery.next().unwrap();
    let mut successor = Reporter::new(binding(next), L, client(10_000).0).unwrap();
    successor
        .prepare(Some(sample(10_000)), client(10_000).0, BASE_SOURCE_AGE_US)
        .unwrap();
    assert_eq!(
        successor
            .queued_receipt(client(10_000).0)
            .unwrap()
            .sequence(),
        1
    );
    assert_eq!(
        f.input
            .bind_readiness_reporter(successor.admitted_cursor(), client(10_000)),
        Err(Error::Stopped(StopReason::InvalidTicket))
    );
    assert_eq!(f.input.suspended_since(), Some(client(10_000)));
}

#[test]
fn the_selected_profile_refuses_downgrades_and_unsolicited_readiness_tickets() {
    for authority in [false, true] {
        let mut f = Fixture::new();
        let proof = record(1, 1, 1_000, f.recovery).authority;
        let result = if authority {
            let mut bytes = vec![0; input_ticket::AUTHORITY_TICKET_BYTES];
            input_ticket::encode_authority(
                proof,
                &mut bytes,
                &L,
                grant().input_channel,
                D::HostToViewer,
                T::Reliable,
            )
            .unwrap();
            f.input
                .accept_authority_ticket(&bytes, correlation(1_000), client(1_000))
        } else {
            let mut bytes = vec![0; input_ticket::INPUT_TICKET_BYTES];
            input_ticket::encode(
                proof.ticket,
                &mut bytes,
                &L,
                grant().input_channel,
                D::HostToViewer,
                T::Reliable,
            )
            .unwrap();
            f.input
                .accept_ticket(&bytes, correlation(1_000), client(1_000))
        };
        assert_eq!(result, Err(Error::Stopped(StopReason::InvalidTicket)));
    }
    let mut input = accepted();
    assert_eq!(
        input.accept_readiness_ticket(
            &encode(record(1, 1, 1_000, RecoveryGeneration::INITIAL)),
            correlation(1_000),
            client(1_000),
        ),
        Err(Error::Stopped(StopReason::InvalidTicket))
    );
}

#[test]
fn explicit_recovery_resets_report_sequence_without_reusing_old_visibility_or_input_ids() {
    let mut f = Fixture::new();
    assert_eq!(f.positive(50_000), 2);
    f.ack(1, 2, 50_100).unwrap();
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    assert_eq!(
        f.input
            .action(press(), &mut out, client(50_101))
            .unwrap()
            .sequence,
        0
    );
    f.input.suspend(client(60_000));
    assert_eq!(f.negative(60_000), 3);
    f.show(70_000);
    let next = f.recovery.next().unwrap();
    f.input.advance_recovery(next, client(71_000)).unwrap();
    assert_eq!(f.input.ticket_deadline(), None);
    assert_eq!(f.input.suspended_since(), Some(client(60_000)));
    f.recovery = next;
    f.serial = 0;
    f.reporter = Reporter::new(binding(next), L, client(71_000).0).unwrap();
    f.input
        .bind_readiness_reporter(f.reporter.admitted_cursor(), client(71_000))
        .unwrap();
    assert_eq!(f.positive(72_000), 1);
    f.ack(2, 1, 72_100).unwrap();
    // The old generation briefly became visible during the same suspension.
    // That picture cannot satisfy the successor's ticket, even with an ack.
    f.blocked(72_100);
    f.show(72_101);
    assert_eq!(f.input.suspended_since(), None);
    let next_action = f.input.action(press(), &mut out, client(72_102)).unwrap();
    let request = decode(&out[..next_action.bytes]);
    assert_eq!(next_action.sequence, 1);
    assert_eq!(request.credentials.view.recovery, next);
    assert_eq!(request.credentials.lease, grant().lease);
    assert_eq!(f.input.pending_actions(), 2);
}

#[test]
fn importing_a_negative_recovery_cursor_requires_a_new_admitted_loss_barrier() {
    let mut f = Fixture::new();
    f.input.suspend(client(10_000));
    let next = f.recovery.next().unwrap();
    f.input.advance_recovery(next, client(20_000)).unwrap();
    f.recovery = next;
    f.serial = 0;
    f.reporter = Reporter::new(binding(next), L, client(20_000).0).unwrap();
    f.reporter
        .prepare(Some(sample(20_000)), client(20_000).0, BASE_SOURCE_AGE_US)
        .unwrap();
    assert_eq!(
        f.reporter
            .queued_receipt(client(20_000).0)
            .unwrap()
            .sequence(),
        1
    );
    f.reporter.force_unavailable(client(20_001).0).unwrap();
    assert_eq!(
        f.reporter
            .queued_receipt(client(20_001).0)
            .unwrap()
            .sequence(),
        2
    );
    f.input
        .bind_readiness_reporter(f.reporter.admitted_cursor(), client(20_002))
        .unwrap();
    assert!(f.input.needs_loss_report());
    f.ack(1, 1, 20_003).unwrap();
    f.show(20_004);
    f.blocked(20_004);
    assert_eq!(f.input.suspended_since(), Some(client(10_000)));
    assert!(f.input.needs_loss_report());
    assert_eq!(f.negative(20_005), 3);
    f.show(70_005);
    assert_eq!(f.positive(70_005), 4);
    f.ack(2, 4, 70_006).unwrap();
    assert_eq!(f.input.suspended_since(), None);
    assert_eq!(f.input.suspension_totals(client(70_006)), (1, 60_006));
}

#[test]
fn an_obsolete_generation_ticket_drains_before_the_successors_acknowledgement_check() {
    for replay in [false, true] {
        let mut f = Fixture::new();
        assert_eq!(f.positive(50_000), 2);
        f.ack(1, 2, 50_100).unwrap();
        // This ticket acknowledges a genuinely admitted old positive report.
        // Its reliable feedback is delayed while reference recovery advances.
        let delayed = encode(record(2, 2, 55_000, f.recovery));
        f.input.suspend(client(60_000));
        assert_eq!(f.negative(60_000), 3);
        let next = f.recovery.next().unwrap();
        f.input.advance_recovery(next, client(70_000)).unwrap();
        f.recovery = next;
        f.serial = 0;
        f.reporter = Reporter::new(binding(next), L, client(70_000).0).unwrap();
        f.input
            .bind_readiness_reporter(f.reporter.admitted_cursor(), client(70_000))
            .unwrap();
        assert_eq!(f.positive(71_000), 1);
        f.show(71_000);
        f.input
            .accept_readiness_ticket(&delayed, correlation(71_100), client(71_100))
            .unwrap();
        assert_eq!(f.input.ticket_deadline(), None);
        assert_eq!(f.input.suspended_since(), Some(client(60_000)));
        f.blocked(71_100);
        if replay {
            // Obsolete feedback still consumes its ticket replay sequence.
            assert_eq!(
                f.ack(2, 1, 71_101),
                Err(Error::Stopped(StopReason::InvalidTicket))
            );
        } else {
            // Old report 2 exceeds the successor's admitted ceiling of 1,
            // but it neither fails that check nor contaminates the new floor.
            f.ack(3, 1, 71_101).unwrap();
            assert_eq!(f.input.suspended_since(), None);
            let mut out = [0; MAX_INPUT_RECORD_BYTES];
            let sent = f.input.action(press(), &mut out, client(71_102)).unwrap();
            let request = decode(&out[..sent.bytes]);
            assert_eq!(sent.sequence, 0);
            assert_eq!(request.credentials.view.recovery, next);
            assert_eq!(request.credentials.ticket, InputTicketId::from_raw(103));
        }
    }
}
