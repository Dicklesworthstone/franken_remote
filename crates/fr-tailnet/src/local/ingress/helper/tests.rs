//! Codec, root-configuration, validation and generation-fencing checks, plus the
//! real connection handler over a Unix socket pair for the paths that refuse
//! before any tool runs. Kernel/nftables enforcement is qualified separately in
//! a private network namespace (`examples/qualify_ingress_helper.rs`).
use super::{
    server::{
        Lifecycle, MAX_CONNECTIONS, Observed, Shared, accept, check_assigned, check_request,
        clear_orphans, connection, ownership::Tables,
    },
    *,
};
use crate::local::ingress::Protocols;
use asupersync::{cx::Cx, net::unix::UnixStream, runtime::RuntimeBuilder};
use serde_json::json;
use std::{
    io::{Read, Write},
    net::IpAddr,
    os::unix::net::UnixStream as StdStream,
    sync::Mutex,
    thread,
    time::Duration,
};

const TABLE: &str = "frdh_0123456789abcdef0123456789abcdef";

fn install(address: &str, port: u16) -> Install {
    Install {
        interface: "tailscale0".into(),
        address: address.parse().unwrap(),
        port,
        protocols: Protocols::UDP_TCP,
    }
}
fn body(frame: &[u8]) -> &[u8] {
    &frame[2..]
}
fn settings() -> Settings {
    Settings::parse(br#"{"interface":"tailscale0","allowed_uids":[1000]}"#).unwrap()
}

#[test]
fn request_codec_round_trips_every_operation() {
    for request in [
        Request::Install(install("100.64.0.1", 8443)),
        Request::Install(Install {
            protocols: Protocols::UDP,
            ..install("fd7a:115c:a1e0::1", 1)
        }),
        Request::Renew { generation: 7 },
        Request::Remove {
            generation: u64::MAX,
        },
    ] {
        let frame = encode_request(&request);
        assert_eq!(
            usize::from(u16::from_be_bytes([frame[0], frame[1]])),
            frame.len() - 2
        );
        assert!(frame.len() - 2 <= MAX_REQUEST);
        assert_eq!(decode_request(body(&frame)), Ok(request));
    }
}

#[test]
fn request_codec_rejects_malformed_oversized_and_unknown_requests() {
    let v4 = encode_request(&Request::Install(install("100.64.0.1", 8443)));
    let v4 = body(&v4).to_vec();
    let with = |index: usize, value: u8| {
        let mut bytes = v4.clone();
        bytes[index] = value;
        bytes
    };
    let mut trailing = v4.clone();
    trailing.push(0);
    let mut long_name = v4[..10].to_vec();
    long_name.push(16);
    long_name.extend(b"abcdefghijklmnop");
    let mut injected = v4[..10].to_vec();
    injected.push(10);
    injected.extend(b"tun0;flush");
    let mut not_utf8 = v4[..10].to_vec();
    not_utf8.extend([2, 0xff, 0xfe]);
    for (bytes, reason) in [
        (vec![], Refusal::Malformed),
        (vec![VERSION], Refusal::Malformed),
        (vec![VERSION, 1], Refusal::Malformed),
        (v4[..9].to_vec(), Refusal::Malformed),
        (with(5, 5), Refusal::Malformed),
        (with(10, 9), Refusal::Malformed),
        (with(10, 11), Refusal::Malformed),
        (trailing, Refusal::Malformed),
        (long_name, Refusal::Malformed),
        (injected, Refusal::Malformed),
        (not_utf8, Refusal::Malformed),
        (vec![VERSION, 2, 0, 0, 0, 0, 0, 0, 0], Refusal::Malformed),
        (
            vec![VERSION, 3, 0, 0, 0, 0, 0, 0, 0, 1, 0],
            Refusal::Malformed,
        ),
        (with(2, 0), Refusal::InvalidProtocols),
        (with(2, 4), Refusal::InvalidProtocols),
        (with(2, 0xff), Refusal::InvalidProtocols),
        (with(0, 2), Refusal::UnsupportedVersion),
        (
            vec![0, 2, 0, 0, 0, 0, 0, 0, 0, 1],
            Refusal::UnsupportedVersion,
        ),
        (with(1, 0), Refusal::UnknownOperation),
        (with(1, 4), Refusal::UnknownOperation),
        (vec![VERSION, 0xff], Refusal::UnknownOperation),
        (vec![VERSION; MAX_REQUEST + 1], Refusal::Oversized),
    ] {
        assert_eq!(decode_request(&bytes), Err(reason), "{bytes:?}");
    }
}

#[test]
fn response_codec_round_trips_and_refuses_foreign_tables_and_unknown_codes() {
    for response in [
        Response::Installed {
            generation: 3,
            index: 42,
            table: TABLE.into(),
            readback: br#"{"nftables":[]}"#.to_vec(),
        },
        Response::Renewed {
            generation: 3,
            readback: b"{}".to_vec(),
        },
        Response::Removed { generation: 3 },
        Response::Refused(Refusal::StaleGeneration),
    ] {
        let frame = encode_response(&response);
        let length = u32::from_be_bytes(frame[..4].try_into().unwrap());
        assert_eq!(usize::try_from(length).unwrap(), frame.len() - 4);
        assert_eq!(decode_response(&frame[4..]), Ok(response));
    }
    let foreign = encode_response(&Response::Installed {
        generation: 1,
        index: 1,
        table: "frd_0123456789abcdef0123456789abcdef".into(),
        readback: Vec::new(),
    });
    assert_eq!(decode_response(&foreign[4..]), Err(Refusal::ForeignTable));
    for bytes in [
        vec![VERSION, 4, 0],
        vec![VERSION, 4, 200],
        vec![VERSION, 4],
        vec![VERSION, 3, 0, 0, 0, 0, 0, 0, 0, 1, 9],
        vec![VERSION, 1, 0, 0],
    ] {
        assert_eq!(
            decode_response(&bytes),
            Err(Refusal::Malformed),
            "{bytes:?}"
        );
    }
    assert_eq!(
        decode_response(&[VERSION, 9]),
        Err(Refusal::UnknownOperation)
    );
    for reason in Refusal::ALL {
        assert_eq!(Refusal::from_code(reason.code()), Some(reason));
        assert_ne!(reason.as_str(), "");
    }
}

#[test]
fn root_configuration_is_strict_and_never_falls_back() {
    let parsed = settings();
    assert_eq!(parsed.interface(), "tailscale0");
    assert_eq!(parsed.socket(), std::path::Path::new(DEFAULT_SOCKET));
    assert!(parsed.allows(1000) && !parsed.allows(1001) && !parsed.allows(0));
    let many: Vec<u32> = (1..=17).collect();
    for bad in [
        json!({"interface":"tailscale0"}),
        json!({"interface":"tailscale0","allowed_uids":[]}),
        json!({"interface":"tailscale0","allowed_uids":[1000,1000]}),
        json!({"interface":"tailscale0","allowed_uids":many}),
        json!({"interface":"tailscale0;","allowed_uids":[1000]}),
        json!({"interface":"","allowed_uids":[1000]}),
        json!({"interface":"tailscale0","allowed_uids":[1000],"socket":"helper.sock"}),
        json!({"interface":"tailscale0","allowed_uids":[1000],"socket":"/run/../tmp/x.sock"}),
        json!({"interface":"tailscale0","allowed_uids":[1000],"rules":"flush ruleset"}),
        json!({"interface":"tailscale0","allowed_uids":["1000"]}),
    ] {
        assert!(
            Settings::parse(&serde_json::to_vec(&bad).unwrap()).is_err(),
            "{bad}"
        );
    }
    let duplicate = br#"{"interface":"tailscale0","interface":"eth0","allowed_uids":[1000]}"#;
    assert_eq!(Settings::parse(duplicate), Err(SettingsError::Malformed));
    assert_eq!(Settings::parse(&[b' '; 4097]), Err(SettingsError::TooLarge));
    // A file in a tree the invoking account can write is never root configuration.
    let own = std::env::current_exe().unwrap();
    assert_eq!(Settings::load(&own), Err(SettingsError::Unprotected));
    assert_eq!(
        Settings::load(std::path::Path::new("relative.json")),
        Err(SettingsError::Unprotected)
    );
}

fn report(ip: &str, extra: &serde_json::Value) -> Observed {
    let mut info = json!({"local": ip, "prefixlen": 32});
    if let Some(map) = extra.as_object() {
        for (key, value) in map {
            info[key] = value.clone();
        }
    }
    Observed {
        index: 7,
        report: serde_json::to_vec(
            &json!([{"ifindex":7,"ifname":"tailscale0","addr_info":[info]}]),
        )
        .unwrap(),
    }
}

#[test]
fn validation_refuses_wrong_interface_foreign_address_port_zero_and_foreign_tables() {
    let settings = settings();
    assert_eq!(
        check_request(&settings, &install("100.64.0.1", 8443)),
        Ok(())
    );
    let mut wrong = install("100.64.0.1", 8443);
    wrong.interface = "eth0".into();
    assert_eq!(
        check_request(&settings, &wrong),
        Err(Refusal::InterfaceNotConfigured)
    );
    assert_eq!(
        check_request(&settings, &install("100.64.0.1", 0)),
        Err(Refusal::InvalidPort)
    );
    for address in [
        "127.0.0.1",
        "0.0.0.0",
        "224.0.0.1",
        "::1",
        "::ffff:100.64.0.1",
    ] {
        assert_eq!(
            check_request(&settings, &install(address, 8443)),
            Err(Refusal::InvalidAddress),
            "{address}"
        );
    }
    let assigned: IpAddr = "100.64.0.1".parse().unwrap();
    let none = json!({});
    assert_eq!(
        check_assigned(&settings, assigned, &report("100.64.0.1", &none)),
        Ok(7)
    );
    for observed in [
        report("100.64.0.9", &none),
        report("100.64.0.1", &json!({"tentative": true})),
        report("100.64.0.1", &json!({"dadfailed": true})),
    ] {
        assert_eq!(
            check_assigned(&settings, assigned, &observed),
            Err(Refusal::AddressNotAssigned)
        );
    }
    // The report must be the kernel's own for exactly the configured index/name.
    for rows in [
        json!([{"ifindex":7,"ifname":"eth0","addr_info":[{"local":"100.64.0.1"}]}]),
        json!([{"ifindex":8,"ifname":"tailscale0","addr_info":[{"local":"100.64.0.1"}]}]),
        json!([]),
        json!({"ifindex":7}),
    ] {
        let observed = Observed {
            index: 7,
            report: serde_json::to_vec(&rows).unwrap(),
        };
        assert_eq!(
            check_assigned(&settings, assigned, &observed),
            Err(Refusal::InterfaceUnqualified)
        );
    }
    assert_eq!(owned_table(TABLE), Ok(()));
    for foreign in [
        "frd_0123456789abcdef0123456789abcdef",
        "frdh_0123456789ABCDEF0123456789abcdef",
        "frdh_0123456789abcdef0123456789abcde",
        "frdh_0123456789abcdef0123456789abcdef0",
        "frdh_0123456789abcdef0123456789abcdeg",
        "fr_sentinel",
        "filter",
        "",
    ] {
        assert_eq!(
            owned_table(foreign),
            Err(Refusal::ForeignTable),
            "{foreign}"
        );
    }
}

#[test]
fn generation_fencing_refuses_stale_renew_and_remove() {
    let tables = Tables::new(1);
    let mut rule = Lifecycle::default();
    assert_eq!(rule.current(1).err(), Some(Refusal::NotInstalled));
    rule.installed(1, tables.reserve(TABLE.into()).unwrap());
    assert_eq!(rule.admit_install(), Err(Refusal::AlreadyInstalled));
    assert!(rule.current(1).is_ok());
    for stale in [0, 2, u64::MAX] {
        assert_eq!(rule.current(stale).err(), Some(Refusal::StaleGeneration));
        assert_eq!(rule.removed(stale).err(), Some(Refusal::StaleGeneration));
    }
    let removed = rule.removed(1).unwrap();
    assert_eq!(removed.generation, 1);
    removed.table.removed();
    assert_eq!(rule.removed(1).err(), Some(Refusal::NotInstalled));
    assert_eq!(rule.admit_install(), Ok(()));
    rule.installed(2, tables.reserve(TABLE.into()).unwrap());
    // The previous generation can neither renew nor remove its successor.
    assert_eq!(rule.current(1).err(), Some(Refusal::StaleGeneration));
    assert_eq!(rule.removed(1).err(), Some(Refusal::StaleGeneration));
    assert_eq!(rule.active.as_ref().map(|owned| owned.generation), Some(2));
    assert_eq!(
        rule.current(2)
            .map(|owned| owned.table.name().to_owned())
            .ok(),
        Some(TABLE.into())
    );
}

/// The client half of a socket pair, blocking, for a scripted peer thread.
/// The async original is closed so the peer's drop is the helper's EOF.
fn peer(client: UnixStream) -> StdStream {
    let stream = client.as_std().try_clone().unwrap();
    drop(client);
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
}
fn response(stream: &mut StdStream) -> Response {
    let mut header = [0_u8; 4];
    stream.read_exact(&mut header).unwrap();
    let mut bytes = vec![0; usize::try_from(u32::from_be_bytes(header)).unwrap()];
    stream.read_exact(&mut bytes).unwrap();
    decode_response(&bytes).unwrap()
}
/// End of stream (the helper closed), as opposed to a read timeout.
fn closed(stream: &mut StdStream) -> bool {
    matches!(stream.read(&mut [0_u8; 1]), Ok(0))
}
/// Drive the REAL connection handler against a scripted peer. Returns every
/// response the peer saw and whether the helper closed the connection.
fn converse(
    script: impl FnOnce(&mut StdStream) -> Vec<Response> + Send + 'static,
) -> (Vec<Response>, Vec<Event>) {
    let (server, client) = UnixStream::pair().unwrap();
    let mut stream = peer(client);
    let peer = thread::spawn(move || script(&mut stream));
    let events = Mutex::new(Vec::new());
    let record = |event: Event| events.lock().unwrap().push(event);
    let settings = settings();
    RuntimeBuilder::new()
        .worker_threads(1)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
        .block_on(async {
            let cx = Cx::current().unwrap();
            let shared = Shared::new(&cx, &settings, &record);
            connection(&shared, server, 1000).await;
        });
    (peer.join().unwrap(), events.into_inner().unwrap())
}

#[test]
fn codec_violations_are_answered_with_typed_refusals_and_close_the_connection() {
    for (frame, reason) in [
        (vec![0x01, 0x00], Refusal::Oversized),
        (vec![0xff, 0xff], Refusal::Oversized),
        (vec![0x00, 0x00], Refusal::Malformed),
        (vec![0x00, 0x02, VERSION, 0x09], Refusal::UnknownOperation),
        (vec![0x00, 0x02, 0x07, 0x02], Refusal::UnsupportedVersion),
        (vec![0x00, 0x03, VERSION, 0x01, 0x01], Refusal::Malformed),
    ] {
        let (seen, events) = converse(move |stream| {
            stream.write_all(&frame).unwrap();
            let first = vec![response(stream)];
            // Then the helper closes: no further response, end of stream.
            assert!(closed(stream));
            first
        });
        assert_eq!(seen, vec![Response::Refused(reason)]);
        assert_eq!(events, vec![Event::Refused { uid: 1000, reason }]);
    }
}

#[test]
fn requests_are_admitted_before_any_tool_runs_and_are_rate_limited() {
    let (seen, events) = converse(|stream| {
        let mut wrong = install("100.64.0.1", 8443);
        wrong.interface = "eth0".into();
        let mut requests = vec![
            Request::Install(wrong),
            Request::Install(install("100.64.0.1", 0)),
        ];
        requests.extend((0..10).map(|generation| Request::Renew { generation }));
        // Pipelined: all frames are queued before the first reply is read, so
        // the budget sees them back to back regardless of host load.
        let frames: Vec<u8> = requests.iter().flat_map(encode_request).collect();
        stream.write_all(&frames).unwrap();
        requests.iter().map(|_| response(stream)).collect()
    });
    assert_eq!(
        seen[..4],
        [
            Refusal::InterfaceNotConfigured,
            Refusal::InvalidPort,
            Refusal::NotInstalled,
            Refusal::NotInstalled,
        ]
        .map(Response::Refused)
    );
    // One request in flight at a time; beyond the burst of four, refusal.
    assert!(seen[4..].contains(&Response::Refused(Refusal::RateLimited)));
    assert!(seen[4..].iter().all(|r| matches!(
        r,
        Response::Refused(Refusal::NotInstalled | Refusal::RateLimited)
    )));
    assert!(
        events
            .iter()
            .all(|event| matches!(event, Event::Refused { uid: 1000, .. })),
        "{events:?}"
    );
}

#[test]
fn a_stalled_partial_frame_is_closed_without_holding_the_connection() {
    let (_, events) = converse(|stream| {
        stream.write_all(&[0x00]).unwrap();
        // No response; the helper closes after its one-second frame deadline.
        assert!(closed(stream));
        Vec::new()
    });
    assert_eq!(events, Vec::new());
}

#[test]
fn full_table_custody_refuses_install_before_running_any_tools() {
    let settings = settings();
    let record = |_: Event| {};
    RuntimeBuilder::new()
        .worker_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let cx = Cx::current().unwrap();
            let shared = Shared::new(&cx, &settings, &record);
            for n in 0..4 * MAX_CONNECTIONS {
                let mut rule = Lifecycle::default();
                rule.installed(
                    u64::try_from(n).unwrap() + 1,
                    shared.reserve_table(format!("frdh_{n:032x}")).unwrap(),
                );
                // An abandoned handler still retains its possible kernel rule.
                drop(rule);
            }
            let original: Vec<_> = (0..4 * MAX_CONNECTIONS)
                .map(|n| format!("frdh_{n:032x}"))
                .collect();
            for n in 4 * MAX_CONNECTIONS..100 {
                assert_eq!(
                    shared.reserve_table(format!("frdh_{n:032x}")).unwrap_err(),
                    Refusal::CapacityExhausted
                );
                let failed_cleanup = shared.take_orphans();
                assert_eq!(failed_cleanup.names(), original);
                drop(failed_cleanup);
            }
            // No TUN, root privileges or tool process is needed to refuse full
            // custody. Running interface qualification first would fail this.
            let result = Lifecycle::default()
                .install(&shared, 1000, &install("100.64.0.1", 8443))
                .await;
            assert_eq!(result, Err(Refusal::CapacityExhausted));
            let deleted = shared.take_orphans();
            assert_eq!(deleted.names(), original);
            deleted.removed();
            assert!(shared.reserve_table(TABLE.into()).is_ok());
        });
}

/// A root-owned stand-in for `nft` in the namespace's private /run: its table
/// inventory is tables.json, and its first `fail` scripts exit 1.
const FAKE_NFT: &str = r"#!/usr/bin/python3
import json, pathlib, sys
root = pathlib.Path(__file__).parent
assert pathlib.Path('/run/fr-synthetic-ingress').read_text() == 'synthetic-only\n'
tables = json.loads((root / 'tables.json').read_text())
fail = root / 'fail'
args = sys.argv[1:]
if args == ['-j', 'list', 'tables', 'inet']:
    rows = [{'table': {'family': 'inet', 'name': t, 'handle': i + 1}} for i, t in enumerate(tables)]
    print(json.dumps({'nftables': [{'metainfo': {'json_schema_version': 1}}] + rows}))
elif args == ['-f', '-']:
    script = sys.stdin.read(4096)
    if int(fail.read_text()) > 0:
        fail.write_text(str(int(fail.read_text()) - 1))
        raise SystemExit(1)
    for line in script.splitlines():
        assert line.startswith('delete table inet '), line
        tables.remove(line.removeprefix('delete table inet '))
    (root / 'tables.json').write_text(json.dumps(tables))
else:
    raise SystemExit(2)
";

/// Exercise real helper cleanup and install admission against bounded,
/// explicitly synthetic command failures. No firewall enforcement is claimed.
#[test]
#[ignore = "explicit isolated mount/network namespace as root: scripts/test_linux_serial_lifecycle.sh"]
fn repeated_cleanup_failures_keep_all_rules_and_recover_install_capacity() {
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
    assert_eq!(
        fs::read_to_string("/run/fr-synthetic-ingress").unwrap(),
        "synthetic-only\n"
    );
    let dir = PathBuf::from(format!("/run/fr-helper-capacity-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let names: Vec<_> = (0..4 * MAX_CONNECTIONS)
        .map(|n| format!("frdh_{n:032x}"))
        .collect();
    fs::write(dir.join("tables.json"), json!(&names).to_string()).unwrap();
    fs::write(dir.join("fail"), "1000").unwrap();
    let nft = dir.join("nft");
    fs::write(&nft, FAKE_NFT).unwrap();
    fs::set_permissions(&nft, fs::Permissions::from_mode(0o700)).unwrap();
    let inventory = || -> Vec<String> {
        serde_json::from_slice(&fs::read(dir.join("tables.json")).unwrap()).unwrap()
    };
    let settings = Settings::parse(
        json!({"interface": "fr-fixture", "allowed_uids": [1000], "nft": nft})
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    let record = |_: Event| {};
    RuntimeBuilder::new()
        .worker_threads(1)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
        .block_on(async {
            let cx = Cx::current().unwrap();
            let shared = Shared::new(&cx, &settings, &record);
            let mut active = Lifecycle::default();
            active.installed(1, shared.reserve_table(names[0].clone()).unwrap());
            for (n, name) in names.iter().enumerate().skip(1) {
                let mut stopped = Lifecycle::default();
                stopped.installed(
                    u64::try_from(n).unwrap() + 1,
                    shared.reserve_table(name.clone()).unwrap(),
                );
                stopped.release(&shared).await;
            }
            assert_eq!(inventory(), names);
            let remaining: u32 = fs::read_to_string(dir.join("fail"))
                .unwrap()
                .parse()
                .unwrap();
            assert!(1000 - remaining > 32, "exercise repeated failed retries");
            let mut next = Lifecycle::default();
            assert_eq!(
                next.install(
                    &shared,
                    1000,
                    &Install {
                        interface: "fr-fixture".into(),
                        ..install("100.64.0.1", 8443)
                    }
                )
                .await,
                Err(Refusal::CapacityExhausted),
            );
            let retry = shared.take_orphans();
            assert_eq!(retry.names(), &names[1..]);
            assert_eq!(
                shared.reserve_table(TABLE.into()).unwrap_err(),
                Refusal::CapacityExhausted,
                "cleanup in flight still owns the full budget",
            );
            // The command starts succeeding. The original active broker's
            // rule is not in the batch and remains protected throughout.
            fs::write(dir.join("fail"), "0").unwrap();
            clear_orphans(&shared, retry).await;
            assert_eq!(inventory(), [names[0].clone()]);
            assert!(active.current(1).is_ok());
            assert_eq!(shared.take_orphans().names(), Vec::<String>::new());
            assert!(shared.reserve_table(TABLE.into()).is_ok());
            active.release(&shared).await;
            assert_eq!(inventory(), Vec::<String>::new());
        });
}

#[test]
#[ignore = "explicit isolated mount/network namespace as root: scripts/test_linux_serial_lifecycle.sh"]
fn a_table_a_failed_disconnect_left_is_cleared_when_a_later_connection_ends() {
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
    const OTHER: &str = "frdh_fedcba9876543210fedcba9876543210";
    assert_eq!(
        fs::read_to_string("/run/fr-synthetic-ingress").unwrap(),
        "synthetic-only\n"
    );
    let dir = PathBuf::from(format!("/run/fr-helper-orphans-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(dir.join("tables.json"), json!([TABLE, OTHER]).to_string()).unwrap();
    // The first delete fails, as a stalled nft past its bound would.
    fs::write(dir.join("fail"), "1").unwrap();
    let nft = dir.join("nft");
    fs::write(&nft, FAKE_NFT).unwrap();
    fs::set_permissions(&nft, fs::Permissions::from_mode(0o700)).unwrap();
    let tables = || -> Vec<String> {
        serde_json::from_slice(&fs::read(dir.join("tables.json")).unwrap()).unwrap()
    };
    let settings = Settings::parse(
        json!({"interface": "fr-fixture", "allowed_uids": [1000], "nft": nft})
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    let events = Mutex::new(Vec::new());
    let record = |event: Event| events.lock().unwrap().push(event);
    RuntimeBuilder::new()
        .worker_threads(1)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
        .block_on(async {
            let cx = Cx::current().unwrap();
            let shared = Shared::new(&cx, &settings, &record);
            let mut first = Lifecycle::default();
            first.installed(1, shared.reserve_table(TABLE.into()).unwrap());
            first.release(&shared).await;
            assert_eq!(tables(), [TABLE, OTHER], "the failed delete left it");
            // A later connection ends: its own table goes, then the first's.
            let mut second = Lifecycle::default();
            second.installed(2, shared.reserve_table(OTHER.into()).unwrap());
            second.release(&shared).await;
        });
    assert_eq!(tables(), Vec::<String>::new());
    assert_eq!(
        events.into_inner().unwrap(),
        [
            Event::CleanupFailed {
                table: TABLE.into()
            },
            Event::Removed {
                generation: 2,
                table: OTHER.into(),
                disconnect: true
            },
            Event::OrphanCleared {
                table: TABLE.into()
            },
        ]
    );
}

/// The serving loop retries an orphan by itself. A broker can stay connected
/// for hours, so a table its failed disconnect left waited for a later
/// connection to end, and after tailscaled restarted its stale `iif != index`
/// drop refused the port's tailnet traffic (ts2, 2026-11-19, fr-eny1).
#[test]
#[ignore = "explicit isolated mount/network namespace as root: scripts/test_linux_serial_lifecycle.sh"]
fn a_table_a_failed_disconnect_left_is_cleared_while_no_other_connection_ends() {
    use asupersync::net::unix::UnixListener;
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
    // A live rule of another broker: never an orphan, so never touched.
    const OTHER: &str = "frdh_fedcba9876543210fedcba9876543210";
    assert_eq!(
        fs::read_to_string("/run/fr-synthetic-ingress").unwrap(),
        "synthetic-only\n"
    );
    let dir = PathBuf::from(format!("/run/fr-helper-retry-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(dir.join("tables.json"), json!([TABLE, OTHER]).to_string()).unwrap();
    fs::write(dir.join("fail"), "1").unwrap();
    let nft = dir.join("nft");
    fs::write(&nft, FAKE_NFT).unwrap();
    fs::set_permissions(&nft, fs::Permissions::from_mode(0o700)).unwrap();
    let tables = || -> Vec<String> {
        serde_json::from_slice(&fs::read(dir.join("tables.json")).unwrap()).unwrap()
    };
    let socket = dir.join("helper.sock");
    let settings = Settings::parse(
        json!({"interface": "fr-fixture", "allowed_uids": [1000], "nft": nft, "socket": socket})
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    let events = Mutex::new(Vec::new());
    let record = |event: Event| events.lock().unwrap().push(event);
    RuntimeBuilder::new()
        .worker_threads(1)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
        .block_on(async {
            let cx = Cx::current().unwrap();
            let shared = Shared::new(&cx, &settings, &record);
            let mut first = Lifecycle::default();
            first.installed(1, shared.reserve_table(TABLE.into()).unwrap());
            first.release(&shared).await;
            assert_eq!(tables(), [TABLE, OTHER], "the failed delete left it");
            // Serve with no connection at all for twice the retry period.
            let listener = UnixListener::bind(&socket).await.unwrap();
            let shutdown = asupersync::time::sleep(cx.now(), Duration::from_secs(5));
            accept(&shared, &listener, shutdown).await.unwrap();
        });
    assert_eq!(tables(), [OTHER]);
    assert_eq!(
        events.into_inner().unwrap(),
        [
            Event::CleanupFailed {
                table: TABLE.into()
            },
            Event::OrphanCleared {
                table: TABLE.into()
            },
        ]
    );
}

/// A root-owned stand-in for both `ip` and `nft`. Its interface report answers
/// only once the table read has started, so a renewal that ran the two in
/// turn could never get one. In `fail` mode both reads exit 1; in `slow S`
/// mode the table read answers after S seconds.
const FAKE_RENEW_TOOLS: &str = r"#!/usr/bin/python3
import json, pathlib, sys, time
root = pathlib.Path(__file__).parent
assert pathlib.Path('/run/fr-synthetic-ingress').read_text() == 'synthetic-only\n'
mode = (root / 'mode').read_text()
fail = mode == 'fail'
args = sys.argv[1:]
if args == ['-j', 'address', 'show', 'dev', 'fr-fixture']:
    deadline = time.monotonic() + 0.9
    while not (root / 'table-read').exists():
        if time.monotonic() > deadline:
            raise SystemExit(3)
        time.sleep(0.005)
    if fail:
        raise SystemExit(1)
    print(json.dumps([{'ifindex': 42, 'ifname': 'fr-fixture', 'addr_info': [{'local': '100.64.0.1'}]}]))
elif args[:5] == ['-j', '-n', 'list', 'table', 'inet'] and len(args) == 6:
    (root / 'table-read').write_text('1')
    if fail:
        raise SystemExit(1)
    if mode.startswith('slow '):
        time.sleep(float(mode.split()[1]))
    t = args[5]
    expr = [
        {'match': {'op': '==', 'left': {'payload': {'protocol': 'ip', 'field': 'daddr'}}, 'right': '100.64.0.1'}},
        {'match': {'op': '==', 'left': {'payload': {'protocol': 'udp', 'field': 'dport'}}, 'right': 1}},
        {'match': {'op': '!=', 'left': {'meta': {'key': 'iif'}}, 'right': 42}},
        {'drop': None},
    ]
    print(json.dumps({'nftables': [
        {'metainfo': {'json_schema_version': 1}},
        {'table': {'family': 'inet', 'name': t, 'handle': 1}},
        {'chain': {'family': 'inet', 'table': t, 'name': 'input', 'type': 'filter', 'hook': 'input', 'prio': -310, 'policy': 'accept'}},
        {'rule': {'family': 'inet', 'table': t, 'chain': 'input', 'expr': expr}},
    ]}))
else:
    raise SystemExit(2)
";

/// A root-only directory holding [`FAKE_RENEW_TOOLS`] as both `ip` and `nft`,
/// and the helper settings that use them.
fn renewal_tools(tag: &str) -> (std::path::PathBuf, Settings) {
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
    assert_eq!(
        fs::read_to_string("/run/fr-synthetic-ingress").unwrap(),
        "synthetic-only\n"
    );
    let dir = PathBuf::from(format!("/run/fr-helper-{tag}-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["ip", "nft"] {
        fs::write(dir.join(name), FAKE_RENEW_TOOLS).unwrap();
        fs::set_permissions(dir.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let settings = Settings::parse(
        json!({"interface": "fr-fixture", "allowed_uids": [1000],
            "nft": dir.join("nft"), "ip": dir.join("ip")})
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    (dir, settings)
}
/// One renewal of generation 7 of [`TABLE`] with the tools in `mode`.
fn renew_once(dir: &std::path::Path, settings: &Settings, mode: &str) -> Result<Response, Refusal> {
    std::fs::write(dir.join("mode"), mode).unwrap();
    let _ = std::fs::remove_file(dir.join("table-read"));
    let record = |_: Event| {};
    RuntimeBuilder::new()
        .worker_threads(1)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
        .block_on(async {
            let cx = Cx::current().unwrap();
            let shared = Shared::new(&cx, settings, &record);
            let mut rule = Lifecycle::default();
            rule.installed(7, shared.reserve_table(TABLE.into()).unwrap());
            rule.renew(&shared, 7).await
        })
}

#[test]
#[ignore = "explicit isolated mount/network namespace as root: scripts/test_linux_serial_lifecycle.sh"]
fn a_renewal_reads_the_interface_and_the_table_together_and_judges_the_interface_first() {
    let (dir, settings) = renewal_tools("renew");
    match renew_once(&dir, &settings, "ok") {
        Ok(Response::Renewed { generation: 7, .. }) => {}
        other => panic!("renewal with overlapping reads: {other:?}"),
    }
    // Both reads fail: the table read still ran, and the interface's refusal
    // is the one reported, as when the reads ran in turn.
    assert_eq!(
        renew_once(&dir, &settings, "fail"),
        Err(Refusal::InterfaceUnqualified)
    );
    assert!(dir.join("table-read").exists());
}

/// With a one-second bound, a table read slowed by load refused the renewal
/// and so ended the host (ts2's helper: 180 `firewall_failed` in 19 hours,
/// fr-eny1). A renewal's reads may now run for the broker's remaining
/// snapshot validity, at most 2.5 s; a read past that is still refused.
#[test]
#[ignore = "explicit isolated mount/network namespace as root: scripts/test_linux_serial_lifecycle.sh"]
fn a_renewal_whose_table_read_takes_over_a_second_still_renews() {
    let (dir, settings) = renewal_tools("slow");
    match renew_once(&dir, &settings, "slow 1.6") {
        Ok(Response::Renewed { generation: 7, .. }) => {}
        other => panic!("renewal with a 1.6 s table read: {other:?}"),
    }
    assert_eq!(
        renew_once(&dir, &settings, "slow 3.2"),
        Err(Refusal::FirewallFailed)
    );
}
