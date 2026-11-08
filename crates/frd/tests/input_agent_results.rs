use fr_core::{
    ids::InputTicketId,
    input::*,
    input_sequence::InputOutcome,
    input_submission::{Operation, Refusal},
    limits::ProtocolLimits,
};
use fr_wire::input::{InputDelivery, InputDirection};
use frd::{
    input_agent::{Agent, AuthorityCommand, Error, InputReply, Reply, Route, Seat},
    input_watchdog::{StopReason, host_now},
};
use std::{num::NonZeroU64, sync::atomic::Ordering};

#[path = "support/input_agent.rs"]
#[allow(dead_code)] // Each suite consumes a different subset of the shared fixture.
mod support;
use support::*;

use fr_wire::input_result::{
    INPUT_RESULT_BYTES, InputResult, SequenceSpace, Stage, decode_input_result, encode_input_result,
};
fn record(reply: InputReply) -> InputResult {
    let InputReply::Record(result) = reply else {
        panic!("expected a retained input receipt: {reply:?}")
    };
    let mut b = [0; INPUT_RESULT_BYTES];
    let n = encode_input_result(
        result,
        &mut b,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(n, INPUT_RESULT_BYTES);
    assert_eq!(
        decode_input_result(
            &b,
            &ProtocolLimits::ABSOLUTE,
            result.binding,
            InputDirection::HostToViewer,
            InputDelivery::Reliable
        )
        .unwrap(),
        result
    );
    assert_ne!(result.stage, Stage::Observed);
    result
}
fn input_reply(agent: &mut Agent) -> InputReply {
    let mut result = None;
    eventually(|| {
        result = agent.try_input_result().unwrap();
        result.is_some()
    });
    result.unwrap()
}
#[test]
fn wire_results_retain_original_binding_and_distinguish_pointer_sequence_space() {
    let (rt, cx) = runtime_cx();
    let t = trace();
    let native = t.clone();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(native)),
            clean,
        )
        .unwrap();
    for (event, space) in [
        (key(true), SequenceSpace::Action),
        (
            InputEvent::Pointer {
                position: DesktopPoint { x: 12, y: 34 },
            },
            SequenceSpace::Pointer,
        ),
    ] {
        agent
            .submit(&bytes(0, event), InputDelivery::Reliable)
            .unwrap();
        assert_eq!(
            agent.authority(AuthorityCommand::Ticket(InputTicketId::from_raw(5))),
            Err(Error::Backpressure)
        );
        let value = record(rt.block_on(agent.input_response().unwrap()).unwrap());
        assert_eq!(value.binding.channel, 7);
        assert_eq!(value.binding.session, credentials().session);
        assert_eq!(value.binding.lease, credentials().lease);
        assert_eq!(value.space, space);
        assert_eq!(value.sequence, 0);
        assert_eq!(value.stage, Stage::SubmittedToOs);
        assert_eq!(value.submitted_operations, 1);
    }
    agent.control().stop(StopReason::LocalRevoke);
    assert!(rt.block_on(driver).handoff_safe());
}
#[test]
fn late_receipt_does_not_take_the_successor_controllers_binding() {
    let (rt, cx) = runtime_cx();
    let t = trace();
    let native = t.clone();
    let seat = Seat::default();
    let (mut old, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(native)),
            clean,
        )
        .unwrap();
    old.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    eventually(|| t.held.load(Ordering::Acquire));
    old.control().stop(StopReason::LocalRevoke);
    assert!(Runner::start(rt, driver).finish().handoff_safe());
    let (rt, cx) = runtime_cx();
    let native = trace();
    let (next, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            Route::new(8, ProtocolLimits::ABSOLUTE),
            move || Ok(Sink::new(native)),
            clean,
        )
        .unwrap();
    let value = record(input_reply(&mut old));
    assert_eq!(value.binding.channel, 7);
    assert_eq!(value.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(value.submitted_operations, 1);
    assert!(!next.control().is_stopped());
    next.control().stop(StopReason::LocalRevoke);
    assert!(rt.block_on(driver).handoff_safe());
}
#[test]
fn selecting_input_projection_never_consumes_an_authority_reply() {
    let (rt, cx) = runtime_cx();
    let native = trace();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(native)),
            clean,
        )
        .unwrap();
    agent
        .authority(AuthorityCommand::Ticket(InputTicketId::from_raw(5)))
        .unwrap();
    assert_eq!(agent.try_input_result(), Err(Error::NotInputCommand));
    assert!(matches!(
        agent.input_response(),
        Err(Error::NotInputCommand)
    ));
    assert!(matches!(reply(&mut agent), Reply::Authority(Ok(_))));
    assert_eq!(agent.try_input_result(), Err(Error::NoPendingCommand));
    agent.control().stop(StopReason::LocalRevoke);
    assert!(rt.block_on(driver).handoff_safe());
}
#[test]
fn authority_ticket_uses_the_same_native_mailbox_and_keeps_legacy_replies_distinct() {
    let (rt, cx) = runtime_cx();
    let native = session(&cx, 3_000_000);
    let until = native
        .monitor()
        .authorization_deadline(host_now(&cx).unwrap())
        .unwrap();
    let observed = trace();
    let sink = observed.clone();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(cx, native, route(), move || Ok(Sink::new(sink)), clean)
        .unwrap();
    agent
        .issue_authority_ticket_record(InputTicketId::from_raw(5), 1)
        .unwrap();
    assert_eq!(
        agent.issue_ticket_record(InputTicketId::from_raw(6), 2),
        Err(Error::Backpressure)
    );
    assert_eq!(agent.try_input_result(), Err(Error::NotInputCommand));
    assert!(matches!(
        agent.input_response(),
        Err(Error::NotInputCommand)
    ));
    let Reply::AuthorityTicket(Ok(proof)) = reply(&mut agent) else {
        panic!("the negotiated authority reply must stay in its original slot");
    };
    assert_eq!(proof.authority_until_us, until.as_micros());
    assert_eq!(proof.ticket.sequence, 1);
    assert_eq!(
        proof.ticket.credentials,
        InputCredentials {
            ticket: InputTicketId::from_raw(5),
            ..credentials()
        }
    );
    assert_eq!(
        proof.ticket.expires_at_us,
        (proof.ticket.issued_at_us + 1_500_000).min(proof.authority_until_us)
    );
    assert!(proof.ticket.expires_at_us < proof.authority_until_us);
    agent
        .issue_ticket_record(InputTicketId::from_raw(6), 2)
        .unwrap();
    let Reply::Ticket(Ok(legacy)) = reply(&mut agent) else {
        panic!("the legacy API must retain its original reply type");
    };
    assert_eq!(legacy.sequence, 2);
    assert_eq!(legacy.credentials.ticket, InputTicketId::from_raw(6));
    assert_eq!(observed.effects.lock().unwrap().len(), 0);
    agent.control().stop(StopReason::LocalRevoke);
    assert!(rt.block_on(driver).handoff_safe());
}
#[test]
fn readiness_ticket_refuses_without_applied_proof_and_never_falls_back() {
    let (rt, cx) = runtime_cx();
    let observed = trace();
    let sink = observed.clone();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(sink)),
            clean,
        )
        .unwrap();
    agent
        .issue_readiness_ticket_record(InputTicketId::from_raw(5), 1)
        .unwrap();
    assert_eq!(
        agent.issue_authority_ticket_record(InputTicketId::from_raw(6), 2),
        Err(Error::Backpressure)
    );
    assert_eq!(agent.try_input_result(), Err(Error::NotInputCommand));
    assert_eq!(
        reply(&mut agent),
        Reply::ReadinessTicket(Err(Refusal::Authority(
            fr_core::authority::AuthorityError::ViewUnready
        )))
    );
    assert!(!agent.control().is_stopped());
    // The refused readiness command published no ticket with this ID.
    agent
        .issue_ticket_record(InputTicketId::from_raw(5), 2)
        .unwrap();
    let Reply::Ticket(Ok(legacy)) = reply(&mut agent) else {
        panic!("legacy reply required");
    };
    assert_eq!(legacy.sequence, 2);
    assert_eq!(legacy.credentials.ticket, InputTicketId::from_raw(5));
    assert_eq!(*observed.effects.lock().unwrap(), Vec::<Operation>::new());
    agent.control().stop(StopReason::LocalRevoke);
    assert!(rt.block_on(driver).handoff_safe());
}
#[test]
fn readiness_ticket_samples_applied_provenance_in_the_original_native_mailbox() {
    let (rt, cx) = runtime_cx();
    let native = session_with_presented(&cx, 3_000_000, NonZeroU64::new(19));
    let until = native
        .monitor()
        .authorization_deadline(host_now(&cx).unwrap())
        .unwrap();
    let observed = trace();
    let sink = observed.clone();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(cx, native, route(), move || Ok(Sink::new(sink)), clean)
        .unwrap();
    agent
        .issue_readiness_ticket_record(InputTicketId::from_raw(5), 1)
        .unwrap();
    assert_eq!(
        agent.issue_ticket_record(InputTicketId::from_raw(6), 2),
        Err(Error::Backpressure)
    );
    assert!(matches!(
        agent.input_response(),
        Err(Error::NotInputCommand)
    ));
    let Reply::ReadinessTicket(Ok(proof)) = reply(&mut agent) else {
        panic!("exact readiness reply must remain in its original slot");
    };
    assert_eq!(proof.presented_sequence, 19);
    assert_eq!(proof.authority.authority_until_us, until.as_micros());
    assert_eq!(proof.authority.ticket.sequence, 1);
    assert_eq!(
        proof.authority.ticket.credentials,
        InputCredentials {
            ticket: InputTicketId::from_raw(5),
            ..credentials()
        }
    );
    assert_eq!(
        proof.authority.ticket.expires_at_us,
        (proof.authority.ticket.issued_at_us + 1_500_000).min(until.as_micros())
    );
    assert!(proof.authority.ticket.expires_at_us < until.as_micros());
    assert_eq!(*observed.effects.lock().unwrap(), Vec::<Operation>::new());
    agent.control().stop(StopReason::LocalRevoke);
    assert!(rt.block_on(driver).handoff_safe());
}
#[test]
fn dropping_projected_wait_revokes_but_retains_completed_result_and_context() {
    let (rt, cx) = runtime_cx();
    let t = trace();
    let native = t.clone();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(native)),
            clean,
        )
        .unwrap();
    agent
        .submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    eventually(|| t.held.load(Ordering::Acquire));
    drop(agent.input_response().unwrap());
    assert!(agent.control().is_stopped());
    assert!(rt.block_on(driver).handoff_safe());
    let result = record(input_reply(&mut agent));
    assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(result.binding.channel, 7);
    assert_eq!(t.effects.lock().unwrap().len(), 2);
}
#[test]
fn panic_receipt_preserves_confirmed_text_prefix_in_the_existing_wire_codec() {
    let (rt, cx) = runtime_cx();
    let t = trace();
    let native = t.clone();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || {
                let mut s = Sink::new(native);
                s.panic_on_second = true;
                Ok(s)
            },
            clean,
        )
        .unwrap();
    agent
        .submit(&bytes(0, InputEvent::Text("ab")), InputDelivery::Reliable)
        .unwrap();
    let result = record(input_reply(&mut agent));
    assert_eq!(result.outcome, InputOutcome::EffectUnknown);
    assert_eq!(result.submitted_operations, 1);
    assert!(result.unknown_next_operation);
    assert_eq!(
        t.effects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
    assert!(rt.block_on(driver).handoff_safe());
}
#[test]
fn evicted_receipts_do_not_become_fabricated_zero_effect_wire_results() {
    let (rt, cx) = runtime_cx();
    let t = trace();
    let native = t.clone();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 5_000_000),
            route(),
            move || Ok(Sink::new(native)),
            clean,
        )
        .unwrap();
    for i in 0..100 {
        agent
            .submit(&bytes(i, key(i % 2 == 0)), InputDelivery::Reliable)
            .unwrap();
        let result = record(input_reply(&mut agent));
        assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
    }
    agent
        .submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    assert_eq!(input_reply(&mut agent), InputReply::ConsumedWithoutReceipt);
    assert_eq!(t.effects.lock().unwrap().len(), 100);
    agent.control().stop(StopReason::LocalRevoke);
    assert!(rt.block_on(driver).handoff_safe());
}

#[path = "input_agent_results/held.rs"]
mod held;
