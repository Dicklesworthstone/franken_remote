//! Parser/lifetime regressions. Lifetime fixtures below do NOT install nft rules
//! or qualify ingress. Node metadata travels through the real Unix/HTTP adapter.
use super::*;
use asupersync::{net::unix::UnixStream, runtime::RuntimeBuilder, types::CancelKind};
use serde_json::{Value, json};
use std::{
    io::Write,
    os::unix::net::UnixListener,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
};
fn address() -> SocketAddr {
    "100.64.0.1:4710".parse().unwrap()
}
fn rule(addr: SocketAddr) -> Value {
    let family = if addr.is_ipv4() { "ip" } else { "ip6" };
    json!({"nftables":[
        {"metainfo":{"json_schema_version":1}},
        {"table":{"family":"inet","name":"frd_fixture","handle":1}},
        {"chain":{"family":"inet","table":"frd_fixture","name":"input","type":"filter","hook":"input","prio":-310,"policy":"accept"}},
        {"rule":{"family":"inet","table":"frd_fixture","chain":"input","expr":[
            {"match":{"op":"==","left":{"payload":{"protocol":family,"field":"daddr"}},"right":addr.ip().to_string()}},
            {"match":{"op":"==","left":{"payload":{"protocol":"udp","field":"dport"}},"right":addr.port()}},
            {"match":{"op":"!=","left":{"meta":{"key":"iif"}},"right":42}}, {"drop":null}
        ]}}
    ]})
}
fn valid(value: &Value, addr: SocketAddr) -> Result<(), Error> {
    validate_rule(
        &serde_json::to_vec(value).unwrap(),
        "frd_fixture",
        addr,
        42,
        "tailscale0",
    )
}
#[test]
fn configuration_refuses_wildcards_and_script_or_path_injection() {
    for name in [
        "",
        "../tun0",
        "tun0\nflush ruleset",
        "tun0;",
        "abcdefghijklmnop",
    ] {
        assert!(Configuration::new(address(), name).is_err());
    }
    for a in [
        "0.0.0.0:4710",
        "127.0.0.1:4710",
        "100.64.0.1:0",
        "[::ffff:100.64.0.1]:4710",
        "[ff02::1]:4710",
    ] {
        assert!(Configuration::new(a.parse().unwrap(), "tailscale0").is_err());
    }
    assert!(Configuration::new(address(), "tailscale0").is_ok());
}
#[test]
fn transaction_only_adds_an_exact_drop_rule() {
    let script = install_script("frd_fixture", address(), 42);
    assert_eq!(
        script,
        "create table inet frd_fixture\nadd chain inet frd_fixture input { type filter hook input priority -310; policy accept; }\nadd rule inet frd_fixture input ip daddr 100.64.0.1 udp dport 4710 meta iif != 42 drop\n"
    );
    let v6 = install_script(
        "frd_fixture",
        "[fd7a:115c:a1e0::1]:4710".parse().unwrap(),
        43,
    );
    assert!(v6.contains("ip6 daddr fd7a:115c:a1e0::1 udp dport 4710 meta iif != 43 drop"));
}
#[test]
fn only_exact_ipv4_and_ipv6_readback_is_accepted() {
    for addr in [address(), "[fd7a:115c:a1e0::1]:4710".parse().unwrap()] {
        let expected = rule(addr);
        assert_eq!(valid(&expected, addr), Ok(()));
        for (pointer, value) in [
            ("/nftables/1/table/name", json!("unowned")),
            ("/nftables/1/table/flags", json!(["dormant"])),
            ("/nftables/2/chain/prio", json!(-309)),
            ("/nftables/2/chain/hook", json!("output")),
            ("/nftables/2/chain/policy", json!("drop")),
            ("/nftables/3/rule/expr/0/match/right", json!("100.64.0.3")),
            ("/nftables/3/rule/expr/1/match/right", json!(4711)),
            ("/nftables/3/rule/expr/2/match/right", json!("eth0")),
            ("/nftables/3/rule/expr/2/match/right", json!(43)),
            ("/nftables/3/rule/expr/2/match/op", json!("==")),
            ("/nftables/3/rule/expr/3", json!({"accept":null})),
        ] {
            let mut broken = expected.clone();
            if let Some(slot) = broken.pointer_mut(pointer) {
                *slot = value;
            } else {
                broken["nftables"][1]["table"]["flags"] = value;
            }
            assert_eq!(
                valid(&broken, addr),
                Err(Error::FirewallMismatch),
                "{pointer}"
            );
        }
    }
}
/// Captured from nftables 1.1.6 (`nft -j -n list table`) after applying
/// `install_script` in a scratch network namespace, with the namespace's own
/// names substituted: the kernel's iif index is printed back as the name.
#[test]
fn installed_nftables_prints_the_qualified_interface_by_name() {
    let real = json!({"nftables":[
        {"metainfo":{"version":"1.1.6","release_name":"Commodore Bullmoose #7","json_schema_version":1}},
        {"table":{"family":"inet","name":"frd_fixture","handle":1}},
        {"chain":{"family":"inet","table":"frd_fixture","name":"input","handle":1,"type":"filter","hook":"input","prio":-310,"policy":"accept"}},
        {"rule":{"family":"inet","table":"frd_fixture","chain":"input","handle":2,"expr":[
            {"match":{"op":"==","left":{"payload":{"protocol":"ip","field":"daddr"}},"right":"100.64.0.1"}},
            {"match":{"op":"==","left":{"payload":{"protocol":"udp","field":"dport"}},"right":4710}},
            {"match":{"op":"!=","left":{"meta":{"key":"iif"}},"right":"tailscale0"}},
            {"drop":null}
        ]}}
    ]});
    assert_eq!(valid(&real, address()), Ok(()));
    let bytes = serde_json::to_vec(&real).unwrap();
    assert_eq!(
        validate_rule(&bytes, "frd_fixture", address(), 42, "tailscale1"),
        Err(Error::FirewallMismatch)
    );
}
#[test]
fn extra_rules_and_unknown_objects_never_pass_readback() {
    for row in [
        json!({"set":{}}),
        rule(address())["nftables"][3].clone(),
        json!({"rule":{}}),
    ] {
        let mut data = rule(address());
        data["nftables"].as_array_mut().unwrap().push(row);
        assert_eq!(valid(&data, address()), Err(Error::FirewallMismatch));
    }
    let mut data = rule(address());
    data["nftables"].as_array_mut().unwrap().pop();
    assert_eq!(valid(&data, address()), Err(Error::FirewallMismatch));
    assert_eq!(
        validate_rule(b"not json", "frd_fixture", address(), 42, "tailscale0"),
        Err(Error::FirewallMismatch)
    );
}
#[test]
fn absence_is_only_accepted_from_a_well_formed_table_inventory() {
    assert_eq!(
        table_exists(br#"{"nftables":[]}"#, "frd_fixture"),
        Ok(false)
    );
    assert_eq!(
        table_exists(
            br#"{"nftables":[{"table":{"family":"inet","name":"frd_fixture"}}]}"#,
            "frd_fixture"
        ),
        Ok(true)
    );
    for bytes in [
        b"{}".as_slice(),
        br#"{"nftables":[{}]}"#,
        br#"{"nftables":[{"table":{"name":"x"}}]}"#,
    ] {
        assert_eq!(
            table_exists(bytes, "frd_fixture"),
            Err(Error::FirewallMismatch)
        );
    }
}
#[test]
fn ordinary_interfaces_and_unprotected_executables_refuse() {
    assert!(interface_index("lo").is_err());
    for path in ["nft", "/tmp", "/no-such-fr-ingress-tool"] {
        assert!(protected_executable(Path::new(path)).is_err());
    }
}
fn runtime() -> asupersync::runtime::Runtime {
    RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
async fn fixture(cx: &Cx) -> Boundary {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-ingress-lifetime-{}-{}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let listener = UnixListener::bind(&path).unwrap();
    let worker = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = Vec::new();
            let mut b = [0];
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 2048);
                stream.read_exact(&mut b).unwrap();
                request.push(b[0]);
            }
            let ips = json!(["100.64.0.1"]);
            let status = json!({"Version":"synthetic-lifetime-fixture","BackendState":"Running","TailscaleIPs":ips,
                "CurrentTailnet":{"Name":"test.invalid","MagicDNSSuffix":"fixture.ts.net"},
                "Self":{"ID":"n-host","NodeID":1,"PublicKey":format!("nodekey:{}","1".repeat(64)),"UserID":7,"TailscaleIPs":ips,"InNetworkMap":true,"DNSName":"host.fixture.ts.net."}});
            let data = serde_json::to_vec(&status).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                data.len()
            )
            .unwrap();
            stream.write_all(&data).unwrap();
        }
    });
    let mut api = LocalApi::new(path).unwrap();
    api.daemon_uid = UnixStream::pair().unwrap().0.peer_cred().unwrap().uid;
    let node = api.node_identity(cx).await.unwrap();
    worker.join().unwrap();
    Boundary {
        lease: Lease {
            api,
            cx: cx.clone(),
            address: address(),
            state: Arc::new(Mutex::new(State {
                node: Arc::new(node),
                active: true,
            })),
        },
        config: Configuration::new(address(), "fixture-tun").unwrap(),
        index: 42,
        table: "not-an-installed-rule".into(),
        cleaned: false,
    }
}
#[test]
fn closing_and_owner_drop_retire_all_clones_but_wrong_address_does_not() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let owner = fixture(&cx).await;
        let lease = owner.lease().unwrap();
        assert_eq!(
            lease.check("100.64.0.2:4710".parse().unwrap()),
            Err(Error::InvalidConfiguration)
        );
        assert_eq!(lease.check(address()), Ok(()));
        let second = lease.clone();
        drop(owner);
        assert_eq!(lease.check(address()), Err(Error::Closed));
        assert_eq!(second.check(address()), Err(Error::Closed));
    });
}
#[test]
fn cleanup_fences_at_call_time_and_refuses_with_any_retained_transport() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut owner = fixture(&cx).await;
        let lease = owner.lease().unwrap();
        drop(owner.stop(&cx));
        assert!(owner.is_closed());
        assert_eq!(owner.stop(&cx).await, Err(Error::InUse));
        assert!(!owner.cleaned);
        assert_eq!(lease.check(address()), Err(Error::Closed));
    });
}
#[test]
fn abandoning_refresh_without_polling_retires_the_original_lifetime() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut owner = fixture(&cx).await;
        let lease = owner.lease().unwrap();
        drop(owner.revalidate());
        assert_eq!(lease.check(address()), Err(Error::Closed));
    });
}
#[test]
fn supervise_completion_and_unpolled_abandonment_both_retire() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut owner = fixture(&cx).await;
        let lease = owner.lease().unwrap();
        assert_eq!(owner.supervise(async { 17 }).await, Ok(17));
        assert_eq!(lease.check(address()), Err(Error::Closed));
        let mut owner = fixture(&cx).await;
        let lease = owner.lease().unwrap();
        drop(owner.supervise(std::future::pending::<()>()));
        assert_eq!(lease.check(address()), Err(Error::Closed));
    });
}
#[test]
fn caught_application_panic_fences_before_the_supervisor_future_is_dropped() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut owner = fixture(&cx).await;
        let lease = owner.lease().unwrap();
        let mut future = Box::pin(owner.supervise(async {
            panic!("local UI fixture");
        }));
        poll_fn(|task| {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                future.as_mut().poll(task)
            }));
            assert!(caught.is_err());
            assert_eq!(lease.check(address()), Err(Error::Closed));
            Poll::Ready(())
        })
        .await;
        drop(future);
    });
}
#[test]
fn expired_node_proof_is_terminal_without_any_native_io_or_refresh() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut owner = fixture(&cx).await;
        let now = super::super::now(&cx).unwrap();
        Arc::get_mut(&mut owner.lease.state.lock().unwrap().node)
            .unwrap()
            .expires_us = now;
        assert_eq!(
            owner.lease.check(address()),
            Err(Error::Identity(IdentityError::Expired))
        );
        assert_eq!(owner.lease.check(address()), Err(Error::Closed));
        assert_eq!(owner.revalidate().await, Err(Error::Closed));
    });
}
#[test]
fn cancellation_retires_without_leaving_a_renewable_lease() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let owner = fixture(&cx).await;
        let lease = owner.lease().unwrap();
        cx.cancel_fast(CancelKind::User);
        assert_eq!(
            lease.check(address()),
            Err(Error::Identity(IdentityError::Cancelled))
        );
        assert_eq!(lease.check(address()), Err(Error::Closed));
    });
}
