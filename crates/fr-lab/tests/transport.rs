#![forbid(unsafe_code)]

use fr_core::time::HostDuration;
use fr_lab::{Destination, Event, Fault, Limits, Refusal, Scenario};

fn scenario(seed: u64, limits: Limits) -> Result<Scenario, fr_lab::Failure> {
    eprintln!("fr-lab seed={seed}");
    Scenario::new(seed, limits)
}

fn us(value: u64) -> HostDuration {
    HostDuration::from_micros(value)
}

fn replay(seed: u64) -> (Vec<fr_lab::TraceEvent>, Vec<u8>) {
    let mut lab = scenario(seed, Limits::default()).unwrap();
    let mut state = Vec::new();
    for i in 0..40 {
        lab.send(
            Destination::Host,
            &[i],
            Fault::Seeded {
                max_delay: us(1_000),
                loss_per_million: 250_000,
                duplicate_per_million: 500_000,
            },
        )
        .unwrap();
    }
    lab.elapse(us(1_000)).unwrap();
    lab.drain(|delivery| {
        state.push(delivery.payload[0]);
        0
    })
    .unwrap();
    assert!(
        lab.trace().iter().any(|e| matches!(
            e.event,
            Event::Sent {
                first_due: None,
                ..
            }
        )),
        "{lab:?}"
    );
    assert!(
        lab.trace().iter().any(|e| matches!(
            e.event,
            Event::Sent {
                second_due: Some(_),
                ..
            }
        )),
        "{lab:?}"
    );
    assert!(
        state.windows(2).any(|p| p[0] > p[1]),
        "no reordering: {lab:?}"
    );
    (lab.trace().to_vec(), state)
}

#[test]
fn seeded_loss_duplicate_reorder_replays_identically() {
    assert_eq!(replay(31), replay(31), "seed=31");
    assert_ne!(replay(31), replay(32), "seeds=31,32");
}

#[test]
fn scripted_bidirectional_delivery_has_stable_tie_breaks() {
    let mut lab = scenario(33, Limits::default()).unwrap();
    lab.send(Destination::Host, &[1], Fault::after(us(3)))
        .unwrap();
    lab.send(
        Destination::Client,
        &[2],
        Fault::Duplicate {
            first: us(1),
            second: us(3),
        },
    )
    .unwrap();
    lab.send(Destination::Host, &[3], Fault::Drop).unwrap();
    lab.elapse(us(3)).unwrap();
    let mut received = Vec::new();
    lab.drain(|d| {
        received.push((d.to, d.payload[0]));
        7
    })
    .unwrap();
    assert_eq!(
        received,
        [
            (Destination::Client, 2),
            (Destination::Host, 1),
            (Destination::Client, 2)
        ],
        "{lab:?}"
    );
    assert!(
        lab.trace()
            .iter()
            .filter_map(|e| match e.event {
                Event::Delivered { outcome, .. } => Some(outcome),
                _ => None,
            })
            .all(|outcome| outcome == 7),
        "{lab:?}"
    );
}

#[test]
fn duplicate_admission_is_atomic_and_counts_bytes() {
    let limits = Limits {
        max_packets: 2,
        max_queue_bytes: 2 * Scenario::packet_slot_bytes() + 5,
        ..Limits::default()
    };
    let mut lab = scenario(34, limits).unwrap();
    let error = lab
        .send(
            Destination::Host,
            b"abc",
            Fault::Duplicate {
                first: us(0),
                second: us(1),
            },
        )
        .unwrap_err();
    assert_eq!(error.reason, Refusal::ByteBudget, "{error}");
    assert_eq!(lab.metrics().queued_packets, 0, "{lab:?}");
    assert_eq!(
        lab.metrics().queue_bytes,
        2 * Scenario::packet_slot_bytes(),
        "{lab:?}"
    );
    lab.send(Destination::Host, b"abcde", Fault::after(us(0)))
        .unwrap();
    assert_eq!(lab.metrics().queue_bytes, limits.max_queue_bytes, "{lab:?}");
    lab.drain(|_| 0).unwrap();
    lab.send(Destination::Client, b"abcde", Fault::after(us(0)))
        .unwrap();
    assert_eq!(
        lab.metrics().peak_queue_bytes,
        limits.max_queue_bytes,
        "{lab:?}"
    );
}

#[test]
fn empty_payloads_consume_slots_and_metadata() {
    let mut lab = scenario(
        35,
        Limits {
            max_packets: 1,
            max_queue_bytes: Scenario::packet_slot_bytes(),
            ..Limits::default()
        },
    )
    .unwrap();
    lab.send(Destination::Host, b"", Fault::after(us(1)))
        .unwrap();
    let error = lab
        .send(Destination::Host, b"", Fault::after(us(1)))
        .unwrap_err();
    assert_eq!(error.reason, Refusal::PacketBudget, "{error}");
    assert!(lab.metrics().queue_bytes > 0, "{lab:?}");
    lab.elapse(us(1)).unwrap();
    lab.drain(|_| 0).unwrap();
    assert_eq!(
        lab.metrics().queue_bytes,
        Scenario::packet_slot_bytes(),
        "{lab:?}"
    );
}

#[test]
fn trace_exhaustion_neither_advances_time_nor_hides_pending_delivery() {
    let mut lab = scenario(
        36,
        Limits {
            max_trace_events: 1,
            ..Limits::default()
        },
    )
    .unwrap();
    lab.send(Destination::Host, b"", Fault::after(us(0)))
        .unwrap();
    let before = lab.now();
    let error = lab.elapse(us(10)).unwrap_err();
    assert_eq!(error.seed, 36);
    assert_eq!(error.reason, Refusal::TraceBudget, "{error}");
    assert_eq!(error.trace.len(), 1, "{error}");
    assert_eq!(lab.now(), before, "{lab:?}");
    assert_eq!(
        lab.drain(|_| panic!("seed=36 callback must not run"))
            .unwrap_err()
            .reason,
        Refusal::TraceBudget
    );
    assert_eq!(lab.metrics().queued_packets, 1, "{lab:?}");
}

#[test]
fn invalid_faults_lengths_and_overflow_refuse_before_allocation() {
    let mut lab = scenario(
        37,
        Limits {
            max_packet_bytes: 1,
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(
        lab.send(Destination::Host, &[0, 1], Fault::Drop)
            .unwrap_err()
            .reason,
        Refusal::PacketTooLarge
    );
    assert_eq!(
        lab.send(
            Destination::Host,
            &[],
            Fault::Seeded {
                max_delay: us(1),
                loss_per_million: 1_000_001,
                duplicate_per_million: 0
            }
        )
        .unwrap_err()
        .reason,
        Refusal::InvalidFault
    );
    assert_eq!(
        lab.elapse(us(u64::MAX)).unwrap_err().reason,
        Refusal::ClockOverflow
    );
    assert_eq!(
        lab.send(Destination::Host, &[], Fault::after(us(u64::MAX)))
            .unwrap_err()
            .reason,
        Refusal::ClockOverflow
    );
    assert!(lab.trace().is_empty(), "{lab:?}");
    assert_eq!(lab.metrics().queued_packets, 0, "{lab:?}");
    assert_eq!(
        scenario(
            37,
            Limits {
                max_packets: usize::MAX,
                ..Limits::default()
            }
        )
        .unwrap_err()
        .reason,
        Refusal::InvalidLimits
    );
}

#[test]
fn cancellation_and_reconnect_reject_already_committed_old_datagrams() {
    let mut lab = scenario(38, Limits::default()).unwrap();
    lab.send(
        Destination::Host,
        b"old",
        Fault::Duplicate {
            first: us(1),
            second: us(2),
        },
    )
    .unwrap();
    lab.fence().unwrap();
    assert_eq!(
        lab.send(Destination::Host, b"no", Fault::Drop)
            .unwrap_err()
            .reason,
        Refusal::ChannelClosed
    );
    lab.reopen().unwrap();
    lab.send(Destination::Host, b"new", Fault::after(us(2)))
        .unwrap();
    lab.elapse(us(2)).unwrap();
    let mut count = 0;
    lab.drain(|d| {
        assert_eq!(d.payload, b"new", "seed=38");
        count += 1;
        0
    })
    .unwrap();
    assert_eq!(count, 1, "{lab:?}");
    assert_eq!(
        lab.trace()
            .iter()
            .filter(|e| matches!(e.event, Event::Stale { .. }))
            .count(),
        2,
        "{lab:?}"
    );
}

#[test]
fn receiver_panic_consumes_uncertain_packet_and_closes_transport() {
    let mut lab = scenario(39, Limits::default()).unwrap();
    lab.send(Destination::Host, b"uncertain-effect", Fault::after(us(0)))
        .unwrap();
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        lab.drain(|_| panic!("seed=39 injected receiver crash")) // ubs:ignore — caught crash proves uncertain effects never replay
    }));
    assert!(crashed.is_err(), "{lab:?}");
    assert!(
        matches!(lab.trace().last().unwrap().event, Event::Dispatching { .. }),
        "unknown callback effects lost from schedule: {lab:?}"
    );
    assert_eq!(lab.metrics().queued_packets, 0, "{lab:?}");
    assert_eq!(
        lab.send(Destination::Host, b"retry", Fault::Drop)
            .unwrap_err()
            .reason,
        Refusal::ChannelClosed
    );
    lab.reopen().unwrap();
    assert_eq!(
        lab.drain(|_| panic!("seed=39 uncertain effects cannot replay"))
            .unwrap(),
        0
    );
}

#[test]
fn debug_and_failure_output_exclude_payloads() {
    let mut lab = scenario(40, Limits::default()).unwrap();
    lab.send(
        Destination::Host,
        b"secret-clipboard-canary",
        Fault::after(us(0)),
    )
    .unwrap();
    let error = lab.elapse(us(u64::MAX)).unwrap_err();
    let diagnostic = format!("{lab:?} {error:?} {error}");
    assert!(
        !diagnostic.contains("secret-clipboard-canary"),
        "seed=40 payload leaked"
    );
    assert!(diagnostic.contains("seed=40"), "seed=40 missing seed");
    assert!(diagnostic.contains("Sent"), "seed=40 missing schedule");
}

#[test]
fn normal_progression_services_each_due_time_with_stable_ties() {
    let mut lab = scenario(41, Limits::default()).unwrap();
    lab.send(Destination::Host, &[1], Fault::after(us(3)))
        .unwrap();
    lab.send(
        Destination::Client,
        &[2],
        Fault::Duplicate {
            first: us(1),
            second: us(3),
        },
    )
    .unwrap();
    let mut received = Vec::new();
    lab.advance(us(4), |d| {
        received.push((d.now.as_micros(), d.payload[0]));
        0
    })
    .unwrap();
    assert_eq!(received, [(1, 2), (3, 1), (3, 2)], "{lab:?}");
    assert_eq!(lab.now().as_micros(), 4, "{lab:?}");
}

#[test]
fn advance_reserves_complete_evidence_before_any_callback() {
    let mut lab = scenario(
        42,
        Limits {
            max_trace_events: 4,
            ..Limits::default()
        },
    )
    .unwrap();
    lab.send(Destination::Host, &[], Fault::after(us(1)))
        .unwrap();
    let mut called = false;
    let error = lab
        .advance(us(2), |_| {
            called = true;
            0
        })
        .unwrap_err();
    assert!(
        !called,
        "seed=42 callback ran without trace budget: {lab:?}"
    );
    assert_eq!(error.reason, Refusal::TraceBudget, "{error}");
    assert_eq!(lab.now().as_micros(), 0, "{lab:?}");
    assert_eq!(lab.metrics().queued_packets, 1, "{lab:?}");
    assert_eq!(lab.trace(), error.trace, "{lab:?}");
}
