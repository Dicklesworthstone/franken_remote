//! The directory read through the real Unix/HTTP fixture daemon, after a real
//! membership admission of the viewer.
use super::*;

/// The admission fixtures, plus the names and addresses a directory needs.
fn named(status: &Value) -> Value {
    let mut status = status.clone();
    status["Self"]["DNSName"] = json!("host.fixture.ts.net.");
    peer_mut(&mut status)["DNSName"] = json!("viewer.fixture.ts.net.");
    let key = format!("nodekey:{}", "3".repeat(64));
    status["Peer"][key.clone()] = json!({"ID":"n-own", "NodeID":3, "PublicKey":key,
        "UserID":7, "DNSName":"own.fixture.ts.net.", "OS":"linux", "Online":true,
        "TailscaleIPs":["100.64.0.3", "fd7a:115c:a1e0::3"], "InNetworkMap":true});
    let key = format!("nodekey:{}", "4".repeat(64));
    status["Peer"][key.clone()] = json!({"ID":"n-other", "NodeID":4, "PublicKey":key,
        "UserID":8, "DNSName":"other.fixture.ts.net.", "Online":true,
        "TailscaleIPs":["100.64.0.4"], "InNetworkMap":true});
    status
}

#[test]
fn an_admitted_viewer_reads_its_own_machines_in_one_status_read() {
    let (status, who) = fixtures();
    let directory_status = named(&status);
    let server = Server::responses(&[&status, &who, &status, &directory_status], Duration::ZERO);
    run_async!(cx, {
        let proof = server
            .client
            .authorize_membership(&cx, endpoints(), GrantPolicy::default())
            .await
            .unwrap();
        let owner = crate::Admission::new(server.client.clone(), cx.clone(), proof).unwrap();
        let directory = owner.directory().await.unwrap();
        assert_eq!(
            server.calls.load(Ordering::SeqCst),
            4,
            "exactly one more read"
        );
        let names: Vec<&str> = directory.nodes.iter().map(|n| n.name.as_str()).collect();
        // The viewer itself and the other user's machine are not listed.
        assert_eq!(names, ["host.fixture.ts.net", "own.fixture.ts.net"]);
        assert!(directory.nodes[0].serving);
        assert_eq!(
            directory.nodes[1].ipv6,
            Some("fd7a:115c:a1e0::3".parse().unwrap())
        );
        assert!(
            owner.lease().observe().is_ok(),
            "reading leaves admission live"
        );
    });
}

#[test]
fn no_directory_is_read_without_a_live_admission_and_bad_reads_are_typed() {
    let (status, who) = fixtures();
    let mut stopped = named(&status);
    stopped["BackendState"] = json!("Stopped");
    let server = Server::responses(&[&status, &who, &status, &stopped], Duration::ZERO);
    run_async!(cx, {
        let proof = server
            .client
            .authorize_membership(&cx, endpoints(), GrantPolicy::default())
            .await
            .unwrap();
        let owner = crate::Admission::new(server.client.clone(), cx.clone(), proof).unwrap();
        assert_eq!(owner.directory().await, Err(Error::BackendNotRunning));
        owner.revoke();
        assert_eq!(owner.directory().await, Err(Error::Revoked));
        assert_eq!(
            server.calls.load(Ordering::SeqCst),
            4,
            "a revoked owner reads nothing"
        );
    });
    // A proof whose host is not the daemon's current Self is refused.
    let (status, who) = fixtures();
    let mut replaced = named(&status);
    replaced["Self"]["ID"] = json!("n-replacement");
    let server = Server::responses(&[&status, &who, &status, &replaced], Duration::ZERO);
    run_async!(cx, {
        let proof = server
            .client
            .authorize_membership(&cx, endpoints(), GrantPolicy::default())
            .await
            .unwrap();
        let owner = crate::Admission::new(server.client.clone(), cx.clone(), proof).unwrap();
        assert_eq!(owner.directory().await, Err(Error::IdentityChanged));
    });
}
