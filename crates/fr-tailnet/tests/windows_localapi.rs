//! Live, on a Windows machine running Tailscale: the installed daemon's
//! named-pipe `LocalAPI` answers the reads a host and a client make.
//! Ignored by default; run on that machine with
//! `cargo test -p fr-tailnet --test windows_localapi -- --ignored`, and
//! `FR_TEST_ROOTS` naming a PEM bundle with the issuing CA's roots for the
//! certificate test. Prints counts and timings only, never names or keys.
#![cfg(windows)]
use asupersync::{cx::Cx, runtime::RuntimeBuilder, types::Budget};
use fr_tailnet::{CertificatePolicy, LocalApi, PeerSelector, trust};
use std::{path::PathBuf, time::Instant};

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
    let roots = PathBuf::from(std::env::var_os("FR_TEST_ROOTS").expect("FR_TEST_ROOTS"));
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
