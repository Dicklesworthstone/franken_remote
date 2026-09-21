#![forbid(unsafe_code)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use fr_client::discovery::{
    DEFAULT_SERVICE_PORT, DIRECTORY_DISCLAIMER, DirectoryHostRecord, DirectoryPolicy,
    DirectoryRefusalReason, DirectoryRequest, DirectoryService, DiscoveredHost, DiscoveryCache,
    HostLink, HostLinkError, INITIAL_BACKOFF_US, MAX_BACKOFF_US, MAX_CONCURRENT_PROBES,
    MAX_SAVED_HOSTS, PeerDiscoveryState, ProbeScheduler, ProbeTarget, SavedHost, SavedHostError,
    SavedHostStore, compute_backoff_us,
};

#[test]
fn test_cache_insertion_and_ttl() {
    let mut cache = DiscoveryCache::with_limits(10_000_000, 10); // 10s TTL
    let host = DiscoveredHost::new(
        "node-1",
        "node-1.tailnet.ts.net",
        vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))],
        8443,
        false,
    );

    let t0 = 1_000_000;
    cache.insert(host.clone(), t0);

    // Immediately available
    assert_eq!(cache.valid_count(t0), 1);
    let retrieved = cache.get("node-1", t0).expect("host should be cached");
    assert_eq!(retrieved.stable_id, "node-1");

    // Still valid before TTL
    let t_valid = t0 + 9_000_000;
    assert!(cache.get("node-1", t_valid).is_some());
    assert_eq!(cache.valid_count(t_valid), 1);

    // Expired after TTL
    let t_expired = t0 + 10_000_001;
    assert!(cache.get("node-1", t_expired).is_none());
    assert_eq!(cache.valid_count(t_expired), 0);
}

#[test]
fn test_cache_tailnet_generation_invalidation() {
    let mut cache = DiscoveryCache::new();
    let host = DiscoveredHost::new(
        "node-work",
        "work.tailnet.ts.net",
        vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 2))],
        8443,
        false,
    );

    let t0 = 100_000;
    cache.insert(host, t0);
    assert_eq!(cache.tailnet_generation(), 1);
    assert!(cache.get("node-work", t0).is_some());

    // Invalidate tailnet (e.g. network interface or tailscale authority change)
    cache.invalidate_tailnet();
    assert_eq!(cache.tailnet_generation(), 2);

    // All entries are instantly invalidated
    assert!(cache.get("node-work", t0).is_none());
    assert_eq!(cache.valid_count(t0), 0);
}

#[test]
fn test_cache_capacity_and_eviction() {
    let max: u8 = 5;
    let mut cache = DiscoveryCache::with_limits(100_000_000, usize::from(max));

    for i in 0u8..max {
        let host = DiscoveredHost::new(
            format!("node-{i}"),
            format!("node-{i}.tailnet.ts.net"),
            vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, i + 10))],
            8443,
            false,
        );
        cache.insert(host, 1_000 + u64::from(i) * 10);
    }

    assert_eq!(cache.raw_count(), 5);

    // Inserting 6th host when full should evict the oldest (node-0)
    let extra = DiscoveredHost::new(
        "node-extra",
        "node-extra.tailnet.ts.net",
        vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 99))],
        8443,
        false,
    );
    cache.insert(extra, 2_000);

    assert_eq!(cache.raw_count(), 5);
    assert!(cache.get("node-0", 2_000).is_none()); // Evicted
    assert!(cache.get("node-extra", 2_000).is_some());
}

#[test]
fn test_non_scanning_target_rejection() {
    // Rejects unspecified (0.0.0.0)
    let unspecified = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    assert!(ProbeTarget::new("node-bad", "bad.ts.net", unspecified, 8443, false).is_err());

    // Rejects broadcast / multicast
    let multicast = IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1));
    assert!(ProbeTarget::new("node-bad", "bad.ts.net", multicast, 8443, false).is_err());

    // Accepts valid unicast tailnet address
    let valid = IpAddr::V4(Ipv4Addr::new(100, 64, 1, 2));
    assert!(ProbeTarget::new("node-good", "good.ts.net", valid, 8443, false).is_ok());
}

#[test]
fn test_probe_scheduler_concurrency_bounding() {
    let mut scheduler = ProbeScheduler::new(MAX_CONCURRENT_PROBES, 2_000_000);
    let cache = DiscoveryCache::new();

    // Enqueue 8 targets
    for i in 0u8..8u8 {
        let target = ProbeTarget::new(
            format!("node-{i}"),
            format!("node-{i}.ts.net"),
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, i + 1)),
            8443,
            false,
        )
        .unwrap();
        assert!(scheduler.enqueue_candidate(target, &cache, 1000));
    }

    assert_eq!(scheduler.queued_count(), 8);

    // Poll up to max concurrent (4)
    let mut in_flight = Vec::new();
    for _ in 0..MAX_CONCURRENT_PROBES {
        if let Some(t) = scheduler.poll_next_probe(1000) {
            in_flight.push(t);
        }
    }
    assert_eq!(in_flight.len(), 4);
    assert_eq!(scheduler.in_flight_count(), 4);
    assert_eq!(scheduler.queued_count(), 4);

    // 5th attempt must return None because concurrency ceiling is saturated
    assert!(scheduler.poll_next_probe(1000).is_none());
}

#[test]
fn test_exponential_backoff_progression() {
    assert_eq!(compute_backoff_us(0), 0);
    assert_eq!(compute_backoff_us(1), INITIAL_BACKOFF_US); // 500 ms
    assert_eq!(compute_backoff_us(2), 1_000_000); // 1 s
    assert_eq!(compute_backoff_us(3), 2_000_000); // 2 s
    assert_eq!(compute_backoff_us(4), 4_000_000); // 4 s
    assert_eq!(compute_backoff_us(5), 8_000_000); // 8 s
    assert_eq!(compute_backoff_us(6), 16_000_000); // 16 s
    assert_eq!(compute_backoff_us(7), MAX_BACKOFF_US); // 30 s capped
    assert_eq!(compute_backoff_us(20), MAX_BACKOFF_US); // 30 s capped
}

#[test]
fn test_probe_failure_and_backoff_enforcement() {
    let mut scheduler = ProbeScheduler::new(MAX_CONCURRENT_PROBES, 2_000_000);
    let mut cache = DiscoveryCache::new();

    let target = ProbeTarget::new(
        "flaky-host",
        "flaky.ts.net",
        IpAddr::V4(Ipv4Addr::new(100, 64, 0, 50)),
        8443,
        false,
    )
    .unwrap();

    let t0 = 10_000_000;
    assert!(scheduler.enqueue_candidate(target.clone(), &cache, t0));
    let probed = scheduler.poll_next_probe(t0).expect("should poll target");
    assert_eq!(probed.stable_id, "flaky-host");

    // Probe fails (port refused / not installed)
    scheduler.on_probe_failure(
        "flaky-host",
        PeerDiscoveryState::NotInstalled,
        &mut cache,
        t0 + 100_000,
    );

    let cached = cache.get("flaky-host", t0 + 100_000).unwrap();
    assert_eq!(cached.state, PeerDiscoveryState::NotInstalled);
    assert_eq!(cached.probe_failures, 1);
    assert_eq!(cached.backoff_until_us, t0 + 100_000 + INITIAL_BACKOFF_US);

    // During backoff, enqueue must be rejected
    assert!(!scheduler.enqueue_candidate(target.clone(), &cache, t0 + 200_000));

    // After backoff expires, enqueue succeeds
    let t_after_backoff = t0 + 100_000 + INITIAL_BACKOFF_US + 1;
    assert!(scheduler.enqueue_candidate(target.clone(), &cache, t_after_backoff));

    // Poll again and succeed
    let _ = scheduler.poll_next_probe(t_after_backoff).unwrap();
    scheduler.on_probe_success(
        "flaky-host",
        PeerDiscoveryState::Ready {
            displays: 1,
            requires_approval: false,
        },
        &mut cache,
        t_after_backoff + 50_000,
    );

    let cached_success = cache.get("flaky-host", t_after_backoff + 50_000).unwrap();
    assert!(cached_success.is_ready());
    assert_eq!(cached_success.probe_failures, 0);
    assert_eq!(cached_success.backoff_until_us, 0);
}

#[test]
fn test_probe_scheduler_timeout_recovery() {
    let mut scheduler = ProbeScheduler::new(MAX_CONCURRENT_PROBES, 2_000_000); // 2s timeout
    let mut cache = DiscoveryCache::new();

    let target = ProbeTarget::new(
        "dead-host",
        "dead.ts.net",
        IpAddr::V4(Ipv4Addr::new(100, 64, 0, 80)),
        8443,
        false,
    )
    .unwrap();

    let t0 = 1_000_000;
    assert!(scheduler.enqueue_candidate(target, &cache, t0));
    let _ = scheduler.poll_next_probe(t0).unwrap();
    assert_eq!(scheduler.in_flight_count(), 1);

    // Check timeouts before deadline
    let timed_out = scheduler.check_timeouts(t0 + 1_999_999, &mut cache);
    assert_eq!(timed_out, Vec::<String>::new());
    assert_eq!(scheduler.in_flight_count(), 1);

    // Check timeouts after deadline (2s)
    let timed_out = scheduler.check_timeouts(t0 + 2_000_000, &mut cache);
    assert_eq!(timed_out, vec!["dead-host"]);
    assert_eq!(scheduler.in_flight_count(), 0);

    // Host should now be recorded as Offline with backoff
    let cached = cache.get("dead-host", t0 + 2_000_000).unwrap();
    assert_eq!(cached.state, PeerDiscoveryState::Offline);
    assert_eq!(cached.probe_failures, 1);
}

#[test]
fn test_host_link_parsing_and_security() {
    // Valid standard link
    let link = HostLink::parse("fr://workstation.tailnet.ts.net").unwrap();
    assert_eq!(link.host, "workstation.tailnet.ts.net");
    assert_eq!(link.port, DEFAULT_SERVICE_PORT);
    assert!(!link.secure);
    assert_eq!(link.display_index, None);

    // Valid with port and display
    let link = HostLink::parse("frs://100.64.0.1:9000?display=2").unwrap();
    assert_eq!(link.host, "100.64.0.1");
    assert_eq!(link.port, 9000);
    assert!(link.secure);
    assert_eq!(link.display_index, Some(2));

    // IPv6 literal
    let link = HostLink::parse("fr://[fd7a:115c:a1e0::1]:8443").unwrap();
    assert_eq!(link.host, "fd7a:115c:a1e0::1");
    assert_eq!(link.port, 8443);

    // SECURITY INVARIANT: Reject userinfo (token@host)
    let err = HostLink::parse("fr://secret-token@host.ts.net").unwrap_err();
    assert_eq!(err, HostLinkError::BearerMaterialForbidden);

    // SECURITY INVARIANT: Reject user:pass@host
    let err = HostLink::parse("fr://alice:password123@host.ts.net").unwrap_err();
    assert_eq!(err, HostLinkError::BearerMaterialForbidden);

    // SECURITY INVARIANT: Reject token parameter in query
    let err = HostLink::parse("fr://host.ts.net?token=bearer123").unwrap_err();
    assert_eq!(err, HostLinkError::BearerMaterialForbidden);

    // SECURITY INVARIANT: Reject auth parameter in query
    let err = HostLink::parse("fr://host.ts.net?auth=somekey").unwrap_err();
    assert_eq!(err, HostLinkError::BearerMaterialForbidden);

    // SECURITY INVARIANT: Reject secret parameter in query
    let err = HostLink::parse("fr://host.ts.net?secret=classified").unwrap_err();
    assert_eq!(err, HostLinkError::BearerMaterialForbidden);

    // Rejection of invalid scheme
    assert_eq!(
        HostLink::parse("https://host.ts.net").unwrap_err(),
        HostLinkError::InvalidScheme
    );

    // Rejection of empty host
    assert_eq!(
        HostLink::parse("fr://").unwrap_err(),
        HostLinkError::MissingHost
    );
}

#[test]
fn test_saved_hosts_store_crud_and_bounds() {
    let mut store = SavedHostStore::new();
    assert!(store.is_empty());

    let host1 = SavedHost::new("id-1", "Office PC", "office.ts.net", 8443, 1000);
    store.add(host1).unwrap();
    assert_eq!(store.len(), 1);

    // Duplicate ID rejected
    let dup_id = SavedHost::new("id-1", "Other", "other.ts.net", 8443, 1000);
    assert_eq!(store.add(dup_id), Err(SavedHostError::DuplicateHost));

    // Duplicate host:port rejected
    let dup_addr = SavedHost::new("id-2", "Office 2", "OFFICE.ts.net", 8443, 1000);
    assert_eq!(store.add(dup_addr), Err(SavedHostError::DuplicateHost));

    // Update connection timestamp
    assert!(store.record_connection("id-1", 2000));
    assert_eq!(store.get("id-1").unwrap().last_connected_us, Some(2000));

    // JSON serialization round-trip
    let json = store.to_json().unwrap();
    let loaded = SavedHostStore::from_json(&json).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded.get("id-1").unwrap().label, "Office PC");

    // Capacity enforcement
    let mut full_store = SavedHostStore::new();
    for i in 0..MAX_SAVED_HOSTS {
        let h = SavedHost::new(
            format!("id-{i}"),
            format!("Host {i}"),
            format!("host-{i}.ts.net"),
            8443,
            1000,
        );
        full_store.add(h).unwrap();
    }
    let overflow = SavedHost::new("id-overflow", "Extra", "extra.ts.net", 8443, 1000);
    assert_eq!(
        full_store.add(overflow),
        Err(SavedHostError::CapacityExceeded)
    );
}

#[test]
fn test_directory_policy_disabled() {
    let mut dir = DirectoryService::new(DirectoryPolicy::Disabled);
    let record = DirectoryHostRecord::new(
        "node-1",
        "node-1.ts.net",
        vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))],
        8443,
        "alice@tailnet",
        true,
    );
    dir.register_host(record).unwrap();

    let req = DirectoryRequest {
        requester_user_id: "alice@tailnet".into(),
        requester_tailnet: "tailnet.ts.net".into(),
    };

    let result = dir.query(&req);
    assert_eq!(result, Err(DirectoryRefusalReason::DirectoryDisabled));
}

#[test]
fn test_directory_policy_restricted_own_user() {
    let policy = DirectoryPolicy::RestrictedOwnUserOnly {
        owner_user_id: "alice@tailnet".into(),
    };
    let mut dir = DirectoryService::new(policy);

    dir.register_host(DirectoryHostRecord::new(
        "node-alice-1",
        "alice-work.ts.net",
        vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 10))],
        8443,
        "alice@tailnet",
        true,
    ))
    .unwrap();

    dir.register_host(DirectoryHostRecord::new(
        "node-bob-1",
        "bob-work.ts.net",
        vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 20))],
        8443,
        "bob@tailnet",
        true,
    ))
    .unwrap();

    // Query from Alice: receives only Alice's records
    let alice_req = DirectoryRequest {
        requester_user_id: "alice@tailnet".into(),
        requester_tailnet: "tailnet.ts.net".into(),
    };
    let resp = dir.query(&alice_req).unwrap();
    assert_eq!(resp.records.len(), 1);
    assert_eq!(resp.records[0].stable_id, "node-alice-1");
    assert_eq!(resp.disclaimer, DIRECTORY_DISCLAIMER);

    // Query from Bob: refused because policy restricts directory to Alice
    let bob_req = DirectoryRequest {
        requester_user_id: "bob@tailnet".into(),
        requester_tailnet: "tailnet.ts.net".into(),
    };
    assert_eq!(
        dir.query(&bob_req),
        Err(DirectoryRefusalReason::RestrictedToOwnUser)
    );

    // Query with unverified identity
    let unverified_req = DirectoryRequest {
        requester_user_id: String::new(),
        requester_tailnet: "tailnet.ts.net".into(),
    };
    assert_eq!(
        dir.query(&unverified_req),
        Err(DirectoryRefusalReason::UnverifiedIdentity)
    );
}

#[test]
fn test_directory_policy_tailnet_members() {
    let policy = DirectoryPolicy::TailnetMembers {
        tailnet_name: "acme-corp.ts.net".into(),
    };
    let mut dir = DirectoryService::new(policy);

    dir.register_host(DirectoryHostRecord::new(
        "server-1",
        "server-1.acme-corp.ts.net",
        vec![IpAddr::V6(Ipv6Addr::LOCALHOST)],
        8443,
        "devops@acme",
        true,
    ))
    .unwrap();

    // Legitimate member of acme-corp
    let member_req = DirectoryRequest {
        requester_user_id: "charlie@acme".into(),
        requester_tailnet: "acme-corp.ts.net".into(),
    };
    let resp = dir.query(&member_req).unwrap();
    assert_eq!(resp.records.len(), 1);
    assert_eq!(resp.records[0].stable_id, "server-1");

    // Outside tailnet member
    let outsider_req = DirectoryRequest {
        requester_user_id: "intruder@other".into(),
        requester_tailnet: "other-net.ts.net".into(),
    };
    assert_eq!(
        dir.query(&outsider_req),
        Err(DirectoryRefusalReason::TailnetMismatch)
    );
}
