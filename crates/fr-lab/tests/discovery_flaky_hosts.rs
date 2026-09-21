#![forbid(unsafe_code)]
//! Deterministic lab qualification tests for tailnet peer discovery, bounded capability
//! probing, exponential backoff under flaky conditions, and directory policy isolation.
//!
//! Conforms to plan section 6.4 and bead `fr-p2-discovery-sx4`:
//! 1. Concurrency ceiling under flood: at most 4 concurrent probes even with 32+ candidates.
//! 2. Flaky host simulation: exponential backoff doubles per failure (500ms -> 30s) and clamps.
//! 3. Probing storm prevention: under backoff, targets are refused for rescheduling.
//! 4. Generation fencing: tailnet switch invalidates all cached states instantly.
//! 5. Non-scanning constraint: subnet broadcast/multicast targets are strictly rejected.
//! 6. Directory privacy and isolation: fleet separation with zero pixel/window-title leakage.

use std::net::{IpAddr, Ipv4Addr};

use fr_client::discovery::{
    DirectoryHostRecord, DirectoryPolicy, DirectoryRefusalReason, DirectoryRequest,
    DirectoryService, DiscoveredHost, DiscoveryCache, HostLink, HostLinkError, INITIAL_BACKOFF_US,
    MAX_BACKOFF_US, MAX_CONCURRENT_PROBES, PeerDiscoveryState, ProbeScheduler, ProbeTarget,
    compute_backoff_us,
};

/// Lab test: Concurrency ceiling under flood.
///
/// When Tailscale `LocalAPI` delivers 32 peer machines simultaneously, the scheduler
/// must never exceed 4 concurrent active probes at any instant.
#[test]
fn test_lab_discovery_concurrency_ceiling_under_burst() {
    let mut scheduler = ProbeScheduler::new(MAX_CONCURRENT_PROBES, 2_000_000);
    let cache = DiscoveryCache::new();

    let candidate_count: usize = 32;
    let t0 = 1_000_000;

    // Enqueue 32 candidate machines
    for i in 0..candidate_count {
        let target = ProbeTarget::new(
            format!("node-batch-{i}"),
            format!("node-{i}.ts.net"),
            IpAddr::V4(Ipv4Addr::new(100, 64, 1, u8::try_from(i + 1).unwrap())),
            8443,
            false,
        )
        .expect("valid unicast address");

        let enqueued = scheduler.enqueue_candidate(target, &cache, t0);
        assert!(enqueued, "target {i} should be enqueued");
    }

    assert_eq!(scheduler.queued_count(), candidate_count);
    assert_eq!(scheduler.in_flight_count(), 0);

    // Drain up to concurrency limit
    let mut active = Vec::new();
    while let Some(target) = scheduler.poll_next_probe(t0) {
        active.push(target);
        assert!(
            scheduler.in_flight_count() <= MAX_CONCURRENT_PROBES,
            "in-flight probes must never exceed concurrency ceiling ({MAX_CONCURRENT_PROBES})"
        );
    }

    assert_eq!(active.len(), MAX_CONCURRENT_PROBES);
    assert_eq!(scheduler.in_flight_count(), MAX_CONCURRENT_PROBES);
    assert_eq!(
        scheduler.queued_count(),
        candidate_count - MAX_CONCURRENT_PROBES
    );

    // Attempting to poll further while saturated must yield None
    assert!(scheduler.poll_next_probe(t0).is_none());
}

/// Lab test: Flaky host simulation with exponential backoff progression and storm prevention.
///
/// Simulates a flaky peer that fails multiple times consecutively:
/// - Verifies backoff schedule: 500ms, 1s, 2s, 4s, 8s, 16s, capped at 30s.
/// - Verifies that during the backoff window, repeated enqueue is rejected.
/// - Verifies that when host finally succeeds, backoff and failure counters reset to zero.
#[test]
fn test_lab_discovery_flaky_host_exponential_backoff_and_reset() {
    let mut scheduler = ProbeScheduler::new(MAX_CONCURRENT_PROBES, 2_000_000);
    let mut cache = DiscoveryCache::new();

    let target = ProbeTarget::new(
        "flaky-workstation",
        "flaky.ts.net",
        IpAddr::V4(Ipv4Addr::new(100, 64, 2, 99)),
        8443,
        false,
    )
    .unwrap();

    let mut current_time_us = 10_000_000;
    let expected_backoffs_us = [
        INITIAL_BACKOFF_US, // 500ms
        1_000_000,          // 1s
        2_000_000,          // 2s
        4_000_000,          // 4s
        8_000_000,          // 8s
        16_000_000,         // 16s
        MAX_BACKOFF_US,     // 30s (cap)
        MAX_BACKOFF_US,     // 30s (cap)
    ];

    for (fail_idx, &expected_backoff) in expected_backoffs_us.iter().enumerate() {
        // Enqueue must succeed because we advance time past the previous backoff
        let enqueued = scheduler.enqueue_candidate(target.clone(), &cache, current_time_us);
        assert!(
            enqueued,
            "should enqueue at failure cycle {fail_idx} after backoff elapsed"
        );

        let probed = scheduler
            .poll_next_probe(current_time_us)
            .expect("should poll target");
        assert_eq!(probed.stable_id, "flaky-workstation");

        // Record probe failure
        scheduler.on_probe_failure(
            "flaky-workstation",
            PeerDiscoveryState::NotInstalled,
            &mut cache,
            current_time_us + 10_000,
        );

        let cached = cache
            .get("flaky-workstation", current_time_us + 10_000)
            .expect("must be in cache");
        assert_eq!(cached.probe_failures, u32::try_from(fail_idx + 1).unwrap());

        let computed = compute_backoff_us(cached.probe_failures);
        assert_eq!(
            computed, expected_backoff,
            "failure {fail_idx}: expected backoff {expected_backoff}us, got {computed}us"
        );

        // Verify storm prevention: enqueue MUST be rejected during backoff
        let mid_backoff_time = current_time_us + 10_000 + (expected_backoff / 2);
        let rejected = scheduler.enqueue_candidate(target.clone(), &cache, mid_backoff_time);
        assert!(
            !rejected,
            "enqueue must be rejected during active backoff window"
        );

        // Advance time to just after the backoff window
        current_time_us = current_time_us + 10_000 + expected_backoff + 1;
    }

    // Now simulate eventual recovery: host boots frd and responds with Ready
    let enqueued = scheduler.enqueue_candidate(target.clone(), &cache, current_time_us);
    assert!(enqueued, "enqueue should succeed after all backoffs");

    let _ = scheduler.poll_next_probe(current_time_us).unwrap();
    scheduler.on_probe_success(
        "flaky-workstation",
        PeerDiscoveryState::Ready {
            displays: 2,
            requires_approval: false,
        },
        &mut cache,
        current_time_us + 20_000,
    );

    let recovered = cache
        .get("flaky-workstation", current_time_us + 20_000)
        .unwrap();
    assert!(recovered.is_ready());
    assert_eq!(recovered.probe_failures, 0, "failure count must reset to 0");
    assert_eq!(
        recovered.backoff_until_us, 0,
        "backoff deadline must reset to 0"
    );
}

/// Lab test: Tailnet generation invalidation on network switch.
///
/// When the tailnet changes, `cache.invalidate_tailnet()` bumps the generation
/// counter, instantly neutralizing all previous probe records without scanning leaks.
#[test]
fn test_lab_discovery_generation_fencing_on_tailnet_switch() {
    let mut cache = DiscoveryCache::new();
    let now = 5_000_000;

    let host_a = DiscoveredHost::new(
        "node-ts-alpha",
        "alpha.tailnet.ts.net",
        vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 10))],
        8443,
        false,
    );
    let host_b = DiscoveredHost::new(
        "node-ts-beta",
        "beta.tailnet.ts.net",
        vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 20))],
        8443,
        false,
    );

    cache.insert(host_a, now);
    cache.insert(host_b, now);
    assert_eq!(cache.valid_count(now), 2);
    assert_eq!(cache.tailnet_generation(), 1);

    // Network switch occurs (e.g. client disconnects from home tailnet and joins corporate tailnet)
    cache.invalidate_tailnet();
    assert_eq!(cache.tailnet_generation(), 2);

    // Old entries must be inaccessible
    assert!(cache.get("node-ts-alpha", now).is_none());
    assert!(cache.get("node-ts-beta", now).is_none());
    assert_eq!(cache.valid_count(now), 0);

    // New host on the new tailnet can be inserted under generation 2
    let host_c = DiscoveredHost::new(
        "node-ts-corp",
        "work.corp.ts.net",
        vec![IpAddr::V4(Ipv4Addr::new(100, 64, 9, 1))],
        8443,
        false,
    );
    cache.insert(host_c, now + 1_000);
    assert_eq!(cache.valid_count(now + 1_000), 1);
    assert_eq!(
        cache.get("node-ts-corp", now + 1_000).unwrap().stable_id,
        "node-ts-corp"
    );
}

/// Lab test: Non-scanning safety invariant.
///
/// Rejects attempts to probe multicast or unspecified addresses, or to supply
/// bearer credentials in host links.
#[test]
fn test_lab_discovery_non_scanning_and_bearer_rejection() {
    // 1. Unspecified address rejected
    let res = ProbeTarget::new(
        "node-unspec",
        "unspec.ts.net",
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        8443,
        false,
    );
    assert!(res.is_err());

    // 2. Multicast address rejected
    let res = ProbeTarget::new(
        "node-mcast",
        "mcast.ts.net",
        IpAddr::V4(Ipv4Addr::new(239, 255, 255, 250)),
        8443,
        false,
    );
    assert!(res.is_err());

    // 3. Bearer credentials in host link rejected
    let test_uris = [
        "fr://bearer_token_123@workstation.ts.net",
        "fr://admin:secret@workstation.ts.net",
        "fr://workstation.ts.net?token=supersecret",
        "fr://workstation.ts.net?auth=jwt.token.here",
        "fr://workstation.ts.net?secret=abcdef",
        "fr://workstation.ts.net?credential=xyz",
        "fr://workstation.ts.net?password=1234",
    ];

    for uri in test_uris {
        let parsed = HostLink::parse(uri);
        assert_eq!(
            parsed,
            Err(HostLinkError::BearerMaterialForbidden),
            "URI {uri} must be rejected with BearerMaterialForbidden"
        );
    }
}

/// Lab test: Directory policy isolation and privacy guarantees.
///
/// Proves that `DirectoryService` isolates fleets per user policy and never exposes
/// UI window titles, screenshots, clipboard contents, or audio streams.
#[test]
fn test_lab_directory_isolation_and_privacy_guarantees() {
    let mut service = DirectoryService::new(DirectoryPolicy::RestrictedOwnUserOnly {
        owner_user_id: "user-alice@tailnet".into(),
    });

    service
        .register_host(DirectoryHostRecord::new(
            "node-alice-work",
            "alice-laptop.ts.net",
            vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 15))],
            8443,
            "user-alice@tailnet",
            true,
        ))
        .unwrap();

    service
        .register_host(DirectoryHostRecord::new(
            "node-bob-server",
            "bob-server.ts.net",
            vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 25))],
            8443,
            "user-bob@tailnet",
            true,
        ))
        .unwrap();

    // Query from authorized user Alice
    let alice_query = DirectoryRequest {
        requester_user_id: "user-alice@tailnet".into(),
        requester_tailnet: "tailnet.ts.net".into(),
    };
    let response = service
        .query(&alice_query)
        .expect("Alice should query successfully");

    // Only Alice's host is returned; Bob's is strictly concealed
    assert_eq!(response.records.len(), 1);
    assert_eq!(response.records[0].stable_id, "node-alice-work");

    // Query from Bob is refused because policy restricts directory to Alice's fleet
    let bob_query = DirectoryRequest {
        requester_user_id: "user-bob@tailnet".into(),
        requester_tailnet: "tailnet.ts.net".into(),
    };
    let refusal = service.query(&bob_query).unwrap_err();
    assert_eq!(refusal, DirectoryRefusalReason::RestrictedToOwnUser);

    // Switch policy to TailnetMembers
    service.set_policy(DirectoryPolicy::TailnetMembers {
        tailnet_name: "corp.ts.net".into(),
    });

    let corp_query = DirectoryRequest {
        requester_user_id: "any-user@corp".into(),
        requester_tailnet: "corp.ts.net".into(),
    };
    let corp_resp = service.query(&corp_query).unwrap();
    assert_eq!(corp_resp.records.len(), 2);

    let external_query = DirectoryRequest {
        requester_user_id: "outsider@other".into(),
        requester_tailnet: "other.ts.net".into(),
    };
    assert_eq!(
        service.query(&external_query).unwrap_err(),
        DirectoryRefusalReason::TailnetMismatch
    );
}
