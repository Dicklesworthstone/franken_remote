//! Cross-socket binding through real credential-checked Unix/HTTP admissions.
use super::*;
use crate::Admission;
use asupersync::time::{TimerDriverHandle, VirtualClock};

fn policy() -> GrantPolicy {
    GrantPolicy {
        lookup_timeout: Duration::from_secs(3),
        validity: Duration::from_secs(3),
        ..Default::default()
    }
}

fn server(values: &[&Value]) -> Server {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-peer-binding-{}-{}-{}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst),
        wall_now().unwrap()
    ));
    Server::start(
        &path,
        values.iter().map(|v| response(v, false)).collect(),
        |_| {},
        |request| {
            assert!(
                request.starts_with("GET /localapi/v0/status?peers=true ")
                    || request.starts_with("GET /localapi/v0/whois?addr=100.64.0.2%3A30001 ")
                    || request.starts_with("GET /localapi/v0/whois?addr=100.64.0.2%3A30002 ")
                    || request.starts_with("GET /localapi/v0/whois?addr=100.64.0.3%3A30001 ")
            );
        },
        true,
    )
}

async fn admit(
    api: &LocalApi,
    cx: &Cx,
    addresses: ConnectionAddresses,
    policy: GrantPolicy,
    membership: bool,
) -> Admission {
    let proof = if membership {
        api.authorize_membership(cx, addresses, policy).await
    } else {
        api.authorize_app_capability(cx, addresses, policy).await
    }
    .unwrap();
    Admission::new(api.clone(), cx.clone(), proof).unwrap()
}

#[test]
fn same_peer_allows_only_the_ephemeral_peer_port_to_differ() {
    let (status, who) = fixtures();
    let server = server(&[
        &status, &who, &status, &status, &who, &status, &status, &who, &status, &status, &who,
        &status,
    ]);
    run_async!(cx, {
        let mut owner = admit(&server.client, &cx, endpoints(), policy(), false).await;
        let mut auxiliary = endpoints();
        auxiliary.peer.set_port(30002);
        let attached = admit(&server.client, &cx, auxiliary, policy(), false).await;
        let first = owner.lease();
        let second = attached.lease();
        assert_eq!(first.same_peer(&first), Ok(true));
        assert_eq!(first.same_peer(&first.clone()), Ok(true));
        assert_eq!(first.same_peer(&second), Ok(true));
        assert_eq!(second.same_peer(&first), Ok(true));
        // Refresh keeps the original binding even though its issue/deadline
        // and Arc allocation change independently of the auxiliary proof.
        owner.refresh().await.unwrap();
        assert_eq!(first.same_peer(&second), Ok(true));
        let mut other_listener = endpoints();
        other_listener.local.set_port(4711);
        let foreign = admit(&server.client, &cx, other_listener, policy(), false).await;
        assert_eq!(first.same_peer(&foreign.lease()), Ok(false));
        assert_eq!(foreign.lease().same_peer(&first), Ok(false));
        assert_eq!(server.calls.load(Ordering::SeqCst), 12);
    });
}

#[test]
fn same_peer_keeps_complete_identity_profile_policy_permissions_and_ip() {
    // Each variant passes production admission on its own. Matching an IP or
    // the same display labels must still fail to attach it to the old session.
    for variant in 0..9 {
        let (status, who) = fixtures();
        let (mut next_status, mut next_who) = (status.clone(), who.clone());
        let mut next_addresses = endpoints();
        let mut next_policy = policy();
        let mut membership = false;
        match variant {
            0 => {
                let key = format!("nodekey:{}", "3".repeat(64));
                let mut peer = peer_mut(&mut next_status).clone();
                peer["ID"] = json!("n-replacement");
                peer["NodeID"] = json!(3);
                peer["PublicKey"] = json!(key);
                next_status["Peer"] = json!({key.clone(): peer});
                next_who["Node"]["ID"] = json!(3);
                next_who["Node"]["StableID"] = json!("n-replacement");
                next_who["Node"]["Key"] = json!(key);
            }
            1 => {
                next_status["Self"]["PublicKey"] = json!(format!("nodekey:{}", "4".repeat(64)));
            }
            2 => membership = true,
            3 => next_policy.scope = Scope::Tailnet,
            4 => next_who["CapMap"][DESKTOP_CAPABILITY][0]["control"] = json!(false),
            5 => {
                peer_mut(&mut next_status)["TailscaleIPs"] = json!(["100.64.0.3"]);
                next_who["Node"]["Addresses"] = json!(["100.64.0.3/32"]);
                next_addresses.peer = "100.64.0.3:30001".parse().unwrap();
            }
            6 => peer_mut(&mut next_status)["DNSName"] = json!("renamed.fixture.ts.net."),
            7 => next_policy.lookup_timeout = Duration::from_secs(2),
            _ => next_policy.validity = Duration::from_secs(2),
        }
        let server = server(&[
            &status,
            &who,
            &status,
            &next_status,
            &next_who,
            &next_status,
        ]);
        run_async!(cx, {
            let owner = admit(&server.client, &cx, endpoints(), policy(), false).await;
            let other = admit(&server.client, &cx, next_addresses, next_policy, membership).await;
            assert!(owner.lease().check().is_ok());
            assert!(other.lease().check().is_ok());
            assert_eq!(
                owner.lease().same_peer(&other.lease()),
                Ok(false),
                "variant {variant}"
            );
            assert_eq!(
                other.lease().same_peer(&owner.lease()),
                Ok(false),
                "variant {variant}"
            );
            assert_eq!(server.calls.load(Ordering::SeqCst), 6);
        });
    }
}

#[test]
fn same_peer_requires_the_original_localapi_authority() {
    let (status, who) = fixtures();
    let first = server(&[&status, &who, &status]);
    let second = server(&[&status, &who, &status]);
    run_async!(cx, {
        let owner = admit(&first.client, &cx, endpoints(), policy(), false).await;
        let other = admit(&second.client, &cx, endpoints(), policy(), false).await;
        assert_eq!(owner.lease().same_peer(&other.lease()), Ok(false));
        assert_eq!(other.lease().same_peer(&owner.lease()), Ok(false));
        assert_eq!(first.calls.load(Ordering::SeqCst), 3);
        assert_eq!(second.calls.load(Ordering::SeqCst), 3);
    });
}

#[test]
fn same_peer_fails_on_either_revoked_or_dropped_owner() {
    for action in 0..3 {
        let (status, who) = fixtures();
        let server = server(&[&status, &who, &status, &status, &who, &status]);
        run_async!(cx, {
            let owner = admit(&server.client, &cx, endpoints(), policy(), false).await;
            let other = admit(&server.client, &cx, endpoints(), policy(), false).await;
            let first = owner.lease();
            let second = other.lease();
            assert_eq!(first.same_peer(&second), Ok(true));
            let expected = match action {
                0 => {
                    owner.revoke();
                    Error::Revoked
                }
                1 => {
                    drop(other);
                    Error::Revoked
                }
                _ => {
                    cx.cancel_fast(CancelKind::User);
                    Error::Cancelled
                }
            };
            assert_eq!(first.same_peer(&second), Err(expected));
            assert_eq!(second.same_peer(&first), Err(expected));
        });
    }
}

#[test]
fn same_peer_stops_at_either_original_deadline_without_lookup() {
    let (status, who) = fixtures();
    let server = server(&[&status, &who, &status, &status, &who, &status]);
    let clock = Arc::new(VirtualClock::starting_at(Time::from_millis(100)));
    RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .with_timer_driver(TimerDriverHandle::with_virtual_clock(clock.clone()))
        .build()
        .unwrap()
        .block_on(async {
            let cx = Cx::current().unwrap();
            let owner = admit(&server.client, &cx, endpoints(), policy(), false).await;
            let until = owner.lease().observe().unwrap();
            clock.advance_to(Time::from_millis(200));
            let other = admit(&server.client, &cx, endpoints(), policy(), false).await;
            assert!(other.lease().observe().unwrap() > until);
            assert_eq!(owner.lease().same_peer(&other.lease()), Ok(true));
            clock.advance_to(Time::from_nanos((until - 1) * 1000));
            assert_eq!(owner.lease().same_peer(&other.lease()), Ok(true));
            clock.advance_to(Time::from_nanos(until * 1000));
            assert!(other.lease().check().is_ok());
            assert_eq!(owner.lease().same_peer(&other.lease()), Err(Error::Expired));
            assert_eq!(other.lease().same_peer(&owner.lease()), Err(Error::Expired));
            assert_eq!(server.calls.load(Ordering::SeqCst), 6);
        });
}
