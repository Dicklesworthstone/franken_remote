//! Live, on a Windows machine running Tailscale: the installed daemon's
//! named-pipe `LocalAPI` answers the reads a host and a client make, and the
//! strong host model protects the tunnel's addresses.
//! Ignored by default; run on that machine with
//! `cargo test -p fr-tailnet --test windows_localapi -- --ignored`. The
//! certificate test checks the pair against the machine's `ROOT` store, or
//! the PEM bundle `FR_TEST_ROOTS` names. Prints counts and timings only,
//! never names or keys.
#![cfg(windows)]
use asupersync::{cx::Cx, runtime::RuntimeBuilder, types::Budget};
use fr_tailnet::{CertificatePolicy, LocalApi, PeerSelector, ingress, trust};
use std::{net::SocketAddr, path::PathBuf, time::Instant};

fn run<T>(work: impl AsyncFnOnce(&Cx) -> T) -> T {
    let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
    let cx = runtime
        .handle()
        .try_request_cx_with_budget(Budget::INFINITE)
        .expect("cx");
    runtime.block_on(async { work(&cx).await })
}

#[test]
#[ignore = "needs the installed Windows tailscaled"]
fn the_installed_daemon_names_this_node_and_pins_a_peer() {
    run(async |cx| {
        let api = LocalApi::installed();
        let started = Instant::now();
        let node = api.node_identity(cx).await.expect("node identity");
        eprintln!(
            "node identity: {} addresses in {:?}",
            node.addresses().len(),
            started.elapsed()
        );
        assert_ne!(node.addresses().len(), 0);
        assert!(node.certificate_name().contains(".ts.net"));

        let started = Instant::now();
        let discovery = api.discover(cx).await.expect("discovery");
        eprintln!(
            "discovery: {} peers in {:?}",
            discovery.peers().len(),
            started.elapsed()
        );
        assert_ne!(discovery.peers().len(), 0);

        // The first peer the daemon will pin (offline peers refuse).
        let mut pinned = 0;
        for peer in discovery.peers() {
            let started = Instant::now();
            if let Ok(target) = api
                .peer_target(cx, PeerSelector::StableId(peer.stable_id()))
                .await
            {
                eprintln!("peer target in {:?}", started.elapsed());
                api.check_peer_target(cx, &target).expect("live target");
                pinned += 1;
                break;
            }
        }
        assert_eq!(pinned, 1, "no peer could be pinned");
    });
}

#[test]
#[ignore = "needs the installed Windows tailscaled with HTTPS certificates"]
fn the_installed_daemon_issues_this_node_its_certificate_pair() {
    let roots = std::env::var_os("FR_TEST_ROOTS")
        .map_or_else(|| PathBuf::from(trust::SYSTEM_BUNDLE), PathBuf::from);
    let roots = trust::root_store(&roots).expect("roots");
    run(async |cx| {
        let api = LocalApi::installed();
        let started = Instant::now();
        let identity = api
            .native_server_identity(cx, roots, CertificatePolicy::default())
            .await
            .expect("certificate pair");
        eprintln!("certificate pair in {:?}", started.elapsed());
        identity.status(cx).expect("credential status");
        identity.stop();
    });
}

#[test]
#[ignore = "needs the installed Windows tailscaled"]
fn the_strong_host_model_protects_the_tunnel_address() {
    run(async |cx| {
        let api = LocalApi::installed();
        for ipv6 in [false, true] {
            assert!(
                ingress::strong_host::enforced(cx, ipv6)
                    .await
                    .expect("interface settings"),
                "ipv6={ipv6}"
            );
        }
        let node = api.node_identity(cx).await.expect("node identity");
        let address = node.addresses()[0];
        let interface = ingress::strong_host::tunnel_interface(cx, address)
            .await
            .expect("tunnel interface");
        let socket = SocketAddr::new(address, 47_001);

        // Another interface's name: refused before anything is leased.
        let wrong = ingress::Configuration::new(socket, "Wi-Fi").expect("configuration");
        assert_eq!(
            ingress::Boundary::install(cx, api.clone(), node, wrong)
                .await
                .err(),
            Some(ingress::Error::UnqualifiedInterface)
        );

        let node = api.node_identity(cx).await.expect("node identity");
        let config = ingress::Configuration::new(socket, &interface).expect("configuration");
        let started = Instant::now();
        let mut boundary = ingress::Boundary::install(cx, api.clone(), node, config)
            .await
            .expect("boundary");
        eprintln!("boundary installed in {:?}", started.elapsed());
        let lease = boundary.lease().expect("lease");
        lease.check(socket).expect("live lease");
        let started = Instant::now();
        boundary.revalidate().await.expect("revalidate");
        eprintln!("boundary revalidated in {:?}", started.elapsed());
        lease.check(socket).expect("lease after revalidation");
        assert_eq!(boundary.stop(cx).await, Err(ingress::Error::InUse));
        drop(lease);
        boundary.stop(cx).await.expect("stop");
    });
}
