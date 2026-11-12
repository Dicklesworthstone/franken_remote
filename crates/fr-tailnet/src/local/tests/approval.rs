//! Approval attribution from the original credential-checked Unix/HTTP proof.
//! The daemon metadata is an explicit fixture, not live Tailscale qualification.
use super::*;
use asupersync::time::{TimerDriverHandle, VirtualClock};

fn named() -> (Value, Value) {
    let (mut status, who) = fixtures();
    status["Self"]["DNSName"] = json!("host.fixture.ts.net.");
    peer_mut(&mut status)["DNSName"] = json!("viewer.fixture.ts.net.");
    (status, who)
}

#[test]
fn approval_identity_uses_the_original_peer_proof_without_reading_or_granting_more() {
    for alias in [Some("viewer.fixture.ts.net."), None] {
        let (mut status, mut who) = fixtures();
        status["Self"]["DNSName"] = json!("different-host.fixture.ts.net.");
        if let Some(alias) = alias {
            peer_mut(&mut status)["DNSName"] = json!(alias);
        }
        who["CapMap"][DESKTOP_CAPABILITY][0]["control"] = json!(false);
        let server = Server::fixture(&status, &who, false);
        run_async!(cx, {
            let proof = server
                .client
                .authorize_app_capability(&cx, endpoints(), GrantPolicy::default())
                .await
                .unwrap();
            let until = proof.expires_us();
            let owner = crate::Admission::new(server.client.clone(), cx, proof).unwrap();
            let lease = owner.lease();
            let identity = lease.approval_identity().unwrap();
            assert_eq!(identity.node_id(), "n-peer");
            assert_eq!(identity.dns_name(), alias);
            assert_eq!(lease.approval_identity(), Ok(identity.clone()));
            assert_eq!(
                format!("{identity:?}"),
                "ApprovalIdentity([verified display labels])"
            );
            assert_eq!(lease.observe(), Ok(until));
            assert_eq!(lease.control(), Err(Error::CapabilityDenied));
            assert_eq!(lease.observe(), Ok(until));
            assert_eq!(server.calls.load(Ordering::SeqCst), 3);
        });
    }
}

#[test]
fn duplicate_daemon_names_keep_distinct_stable_nodes_and_cannot_revive_the_old_lease() {
    let (status, who) = named();
    let mut replacement = status.clone();
    let mut next_who = who.clone();
    let key = format!("nodekey:{}", "3".repeat(64));
    let mut peer = peer_mut(&mut replacement).clone();
    peer["ID"] = json!("n-replacement");
    peer["NodeID"] = json!(3);
    peer["PublicKey"] = json!(key);
    replacement["Peer"] = json!({key.clone(): peer});
    next_who["Node"]["ID"] = json!(3);
    next_who["Node"]["StableID"] = json!("n-replacement");
    next_who["Node"]["Key"] = json!(key);
    let server = Server::responses(
        &[
            &status,
            &who,
            &status,
            &replacement,
            &next_who,
            &replacement,
        ],
        Duration::ZERO,
    );
    run_async!(cx, {
        let proof = server
            .client
            .authorize_membership(&cx, endpoints(), GrantPolicy::default())
            .await
            .unwrap();
        let owner = crate::Admission::new(server.client.clone(), cx.clone(), proof).unwrap();
        let old_lease = owner.lease();
        let old = old_lease.approval_identity().unwrap();
        owner.revoke();

        let proof = server
            .client
            .authorize_membership(&cx, endpoints(), GrantPolicy::default())
            .await
            .unwrap();
        let replacement = crate::Admission::new(server.client.clone(), cx, proof).unwrap();
        let current = replacement.lease().approval_identity().unwrap();
        assert_eq!(old.node_id(), "n-peer");
        assert_eq!(current.node_id(), "n-replacement");
        assert_eq!(old.dns_name(), current.dns_name());
        assert_ne!(old, current);
        assert_eq!(old_lease.approval_identity(), Err(Error::Revoked));
        assert_eq!(old_lease.observe(), Err(Error::Revoked));
        assert!(replacement.lease().observe().is_ok());
        assert_eq!(server.calls.load(Ordering::SeqCst), 6);
    });
}

#[test]
fn approval_identity_stops_on_original_owner_drop_revoke_and_cancellation() {
    for action in 0..3 {
        let (status, who) = named();
        let server = Server::fixture(&status, &who, false);
        run_async!(cx, {
            let proof = server
                .client
                .authorize_membership(&cx, endpoints(), GrantPolicy::default())
                .await
                .unwrap();
            let owner = crate::Admission::new(server.client.clone(), cx.clone(), proof).unwrap();
            let lease = owner.lease();
            let retained = lease.approval_identity().unwrap();
            let expected = match action {
                0 => {
                    drop(owner);
                    Error::Revoked
                }
                1 => {
                    owner.revoke();
                    Error::Revoked
                }
                _ => {
                    cx.cancel_fast(CancelKind::User);
                    Error::Cancelled
                }
            };
            assert_eq!(lease.approval_identity(), Err(expected));
            assert_eq!(lease.clone().approval_identity(), Err(expected));
            assert_eq!(lease.observe(), Err(expected));
            assert_eq!(retained.node_id(), "n-peer");
            assert_eq!(retained.dns_name(), Some("viewer.fixture.ts.net."));
            assert_eq!(server.calls.load(Ordering::SeqCst), 3);
        });
    }
}

#[test]
fn approval_identity_expires_at_the_original_deadline_without_another_lookup() {
    let (status, who) = named();
    let server = Server::fixture(&status, &who, false);
    let clock = Arc::new(VirtualClock::starting_at(Time::from_millis(100)));
    RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .with_timer_driver(TimerDriverHandle::with_virtual_clock(clock.clone()))
        .build()
        .unwrap()
        .block_on(async {
            let cx = Cx::current().unwrap();
            let proof = server
                .client
                .authorize_membership(&cx, endpoints(), GrantPolicy::default())
                .await
                .unwrap();
            let until = proof.expires_us();
            let mut owner = crate::Admission::new(server.client.clone(), cx, proof).unwrap();
            let lease = owner.lease();
            let retained = lease.approval_identity().unwrap();
            clock.advance_to(Time::from_nanos((until - 1) * 1_000));
            assert_eq!(lease.approval_identity(), Ok(retained.clone()));
            assert_eq!(lease.observe(), Ok(until));
            clock.advance_to(Time::from_nanos(until * 1_000));
            assert_eq!(lease.approval_identity(), Err(Error::Expired));
            assert_eq!(owner.refresh().await, Err(Error::Expired));
            assert_eq!(lease.approval_identity(), Err(Error::Expired));
            assert_eq!(retained.node_id(), "n-peer");
            assert_eq!(server.calls.load(Ordering::SeqCst), 3);
        });
}

#[test]
fn identity_refresh_cannot_retarget_a_retained_approval_snapshot() {
    let (status, who) = named();
    let mut renamed = status.clone();
    peer_mut(&mut renamed)["DNSName"] = json!("replacement-name.fixture.ts.net.");
    let server = Server::responses(
        &[&status, &who, &status, &renamed, &who, &renamed],
        Duration::ZERO,
    );
    run_async!(cx, {
        let proof = server
            .client
            .authorize_membership(&cx, endpoints(), GrantPolicy::default())
            .await
            .unwrap();
        let mut owner = crate::Admission::new(server.client.clone(), cx, proof).unwrap();
        let lease = owner.lease();
        let retained = lease.approval_identity().unwrap();
        assert_eq!(owner.refresh().await, Err(Error::IdentityChanged));
        assert_eq!(lease.approval_identity(), Err(Error::IdentityChanged));
        assert_eq!(retained.node_id(), "n-peer");
        assert_eq!(retained.dns_name(), Some("viewer.fixture.ts.net."));
        assert_eq!(server.calls.load(Ordering::SeqCst), 6);
    });
}
