#![forbid(unsafe_code)]
//! Lab flood scenario proving revoke latency stays bounded while a peer floods
//! recovery requests, with logged queue and limit counters (Plan §17.2, §19.3).

use fr_core::authority::{AuthorityPolicy, SessionAuthority};
use fr_core::dos::{DosRefusal, FloodFairQueue, QueueLane, RateLimiterRegistry};
use fr_core::ids::{InputLeaseId, InputTicketId, RemoteSessionId};
use fr_core::limits::{LimitField, ProtocolLimits};
use fr_core::time::HostInstant;

#[derive(Debug, Clone, PartialEq, Eq)]
enum RequestItem {
    RecoveryRequest { seq: u32 },
    PointerUpdate { x: i32, y: i32 },
    RevokeAuthority { lease_id: InputLeaseId },
}

#[derive(Debug, Default)]
struct FloodScenarioCounters {
    bulk_attempted: usize,
    bulk_admitted: usize,
    bulk_refused_rate_limit: usize,
    high_attempted: usize,
    high_admitted: usize,
    high_dispatched: usize,
    peak_queue_count: usize,
    peak_queue_bytes: usize,
    revoke_dispatch_turns: usize,
}

#[test]
fn revoke_latency_stays_bounded_during_recovery_flood() {
    let limits = ProtocolLimits::ABSOLUTE;
    let t0 = HostInstant::ORIGIN;

    // Set up authority with active control
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(42),
        AuthorityPolicy::plan_defaults(),
    );
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(t0).unwrap();
    authority.mark_view_ready(t0).unwrap();
    let lease = InputLeaseId::from_raw(1);
    let ticket = InputTicketId::from_raw(1);
    authority.grant_lease(lease, t0).unwrap();
    authority.issue_input_ticket(lease, ticket, t0).unwrap();
    assert!(authority.has_live_control(t0));

    // Flood-fair queue and rate limiters
    let mut queue = FloodFairQueue::<RequestItem>::from_limits(&limits);
    let mut rate_limiters = RateLimiterRegistry::from_limits(&limits, t0);
    let mut counters = FloodScenarioCounters::default();

    // 1. Peer floods 1,000 recovery requests in Bulk lane at t0
    let recovery_request_payload_bytes = 128_usize;
    for i in 0..1000_u32 {
        counters.bulk_attempted += 1;

        // First check rate limit
        if let Err(refusal) = rate_limiters.check_recovery_request(t0) {
            match refusal {
                DosRefusal::RateLimitExceeded { field, .. } => {
                    assert_eq!(field, LimitField::RecoveryRequestsPerSec);
                    counters.bulk_refused_rate_limit += 1;
                }
                other => panic!("unexpected refusal: {other:?}"),
            }
            continue;
        }

        // Try pushing to Bulk lane of FloodFairQueue
        let item = RequestItem::RecoveryRequest { seq: i };
        match queue.push(QueueLane::Bulk, item, recovery_request_payload_bytes) {
            Ok(()) => {
                counters.bulk_admitted += 1;
            }
            Err(other) => panic!("unexpected queue refusal: {other:?}"),
        }

        counters.peak_queue_count = counters.peak_queue_count.max(queue.len());
        let total_bytes = queue.lane_bytes(QueueLane::High)
            + queue.lane_bytes(QueueLane::Normal)
            + queue.lane_bytes(QueueLane::Bulk);
        counters.peak_queue_bytes = counters.peak_queue_bytes.max(total_bytes);
    }

    // Verify flood was bounded: rate limit and queue capacity kicked in
    assert!(
        counters.bulk_refused_rate_limit > 0,
        "rate limiter must bound flood"
    );
    assert!(
        counters.bulk_admitted > 0,
        "some recovery requests were admitted"
    );
    assert!(
        queue.len() <= 256,
        "bulk queue must stay within count bounds"
    );

    // 2. An emergency revoke request arrives at High priority lane
    counters.high_attempted += 1;
    let revoke_item = RequestItem::RevokeAuthority { lease_id: lease };
    queue
        .push(QueueLane::High, revoke_item.clone(), 32)
        .expect("high priority lane has reserved capacity");
    counters.high_admitted += 1;

    // 3. Measure dispatch latency: High lane MUST be popped immediately on the first pop!
    let mut turns = 0;
    while let Some((lane, item)) = queue.pop() {
        turns += 1;
        if lane == QueueLane::High {
            counters.high_dispatched += 1;
            if let RequestItem::RevokeAuthority { lease_id } = item {
                assert_eq!(lease_id, lease);
                authority.revoke_lease();
                counters.revoke_dispatch_turns = turns;
                break;
            }
        }
    }

    // 4. Invariant assertions
    assert_eq!(
        counters.revoke_dispatch_turns, 1,
        "Revoke MUST be dispatched on turn 1 (bounded latency O(1))"
    );
    assert!(
        !authority.has_live_control(t0),
        "Authority must be revoked immediately"
    );

    // 5. Log all counters for audit evidence
    log_flood_scenario_counters(&counters);
}

fn log_flood_scenario_counters(counters: &FloodScenarioCounters) {
    eprintln!("=== DoS Flood & Revoke Latency Scenario Counters ===");
    eprintln!(
        "Bulk recovery attempted:            {}",
        counters.bulk_attempted
    );
    eprintln!(
        "Bulk recovery admitted to queue:    {}",
        counters.bulk_admitted
    );
    eprintln!(
        "Bulk rate limit rejections:         {}",
        counters.bulk_refused_rate_limit
    );
    eprintln!(
        "High priority attempted:            {}",
        counters.high_attempted
    );
    eprintln!(
        "High priority admitted:             {}",
        counters.high_admitted
    );
    eprintln!(
        "High priority dispatched:           {}",
        counters.high_dispatched
    );
    eprintln!(
        "Revoke dispatch latency (turns):    {}",
        counters.revoke_dispatch_turns
    );
    eprintln!(
        "Peak queue count across all lanes:  {}",
        counters.peak_queue_count
    );
    eprintln!(
        "Peak queue bytes across all lanes:  {}",
        counters.peak_queue_bytes
    );
    eprintln!("=====================================================");
}

#[test]
fn queue_saturation_under_bulk_flood_proves_count_and_byte_bounds() {
    // Sized queue with max 32 items and 4096 bytes on Bulk
    let mut queue = FloodFairQueue::<RequestItem>::new(8, 1024, 16, 2048, 32, 4096);
    let mut count_rejections = 0;
    let mut byte_rejections = 0;
    let mut admitted = 0;

    // Push 100 small items (each 32 bytes) -> should cap at 32 count
    for i in 0..100 {
        match queue.push(QueueLane::Bulk, RequestItem::RecoveryRequest { seq: i }, 32) {
            Ok(()) => admitted += 1,
            Err(DosRefusal::QueueFull {
                lane,
                current_count,
                max_count,
            }) => {
                assert_eq!(lane, QueueLane::Bulk);
                assert_eq!(current_count, 32);
                assert_eq!(max_count, 32);
                count_rejections += 1;
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert_eq!(admitted, 32);
    assert_eq!(count_rejections, 68);

    // Drain the queue
    while queue.pop().is_some() {}

    // Now push 5 large items (each 1024 bytes) -> should cap at 4 items (4096 bytes) and reject 5th on bytes
    let mut byte_admitted = 0;
    for i in 0..5 {
        match queue.push(
            QueueLane::Bulk,
            RequestItem::RecoveryRequest { seq: i },
            1024,
        ) {
            Ok(()) => byte_admitted += 1,
            Err(DosRefusal::ByteLimitExceeded {
                lane,
                current_bytes,
                max_bytes,
            }) => {
                assert_eq!(lane, QueueLane::Bulk);
                assert_eq!(current_bytes, 4096);
                assert_eq!(max_bytes, 4096);
                byte_rejections += 1;
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert_eq!(byte_admitted, 4);
    assert_eq!(byte_rejections, 1);

    eprintln!("Queue Saturation Verification: 68 count rejections, 1 byte rejection under flood");
}

#[test]
fn interleaved_traffic_defends_against_starvation() {
    let limits = ProtocolLimits::ABSOLUTE;
    let mut queue = FloodFairQueue::<RequestItem>::from_limits(&limits);

    // Enqueue 5 Normal requests and 10 Bulk requests
    for i in 0..5 {
        queue
            .push(
                QueueLane::Normal,
                RequestItem::PointerUpdate { x: i, y: i },
                16,
            )
            .unwrap();
    }
    for i in 0..10_u32 {
        queue
            .push(QueueLane::Bulk, RequestItem::RecoveryRequest { seq: i }, 64)
            .unwrap();
    }

    // Normal lane has weight 3, Bulk has weight 1
    // Popping should produce: Normal, Normal, Normal, Bulk, Normal, Normal, Bulk...
    let mut dispatch_order = Vec::new();
    while let Some((lane, _)) = queue.pop() {
        dispatch_order.push(lane);
    }

    assert_eq!(
        &dispatch_order[..4],
        &[
            QueueLane::Normal,
            QueueLane::Normal,
            QueueLane::Normal,
            QueueLane::Bulk
        ],
        "Deficit round-robin must service 3 Normal then 1 Bulk"
    );
}

#[test]
fn single_fifo_planted_negative_proves_starvation_vulnerability() {
    // In a single naive FIFO queue, a high-priority revoke behind 1000 bulk requests
    // suffers unbounded latency of 1000 turns.
    let mut fifo: std::collections::VecDeque<RequestItem> = std::collections::VecDeque::new();

    // 1000 bulk requests enqueued first
    for i in 0..1000_u32 {
        fifo.push_back(RequestItem::RecoveryRequest { seq: i });
    }

    // Revoke enqueued at the back
    let lease = InputLeaseId::from_raw(99);
    fifo.push_back(RequestItem::RevokeAuthority { lease_id: lease });

    // Measure FIFO latency
    let mut turns = 0;
    while let Some(item) = fifo.pop_front() {
        turns += 1;
        if matches!(item, RequestItem::RevokeAuthority { .. }) {
            break;
        }
    }

    // Planted negative: FIFO has 1001 turns latency, whereas FloodFairQueue has 1 turn latency
    assert_eq!(turns, 1001);
    eprintln!(
        "Planted negative verification: Naive FIFO revoke latency was {turns} turns; FloodFairQueue was 1 turn"
    );
}
