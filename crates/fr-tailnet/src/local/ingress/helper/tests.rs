//! Codec, root-configuration, validation and generation-fencing checks, plus the
//! real connection handler over a Unix socket pair for the paths that refuse
//! before any tool runs. Kernel/nftables enforcement is qualified separately in
//! a private network namespace (`examples/qualify_ingress_helper.rs`).
use super::{
    server::{Lifecycle, Observed, Shared, check_assigned, check_request, connection},
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
    let mut rule = Lifecycle::default();
    assert_eq!(rule.current(1).err(), Some(Refusal::NotInstalled));
    rule.installed(1, TABLE);
    assert_eq!(rule.admit_install(), Err(Refusal::AlreadyInstalled));
    assert!(rule.current(1).is_ok());
    for stale in [0, 2, u64::MAX] {
        assert_eq!(rule.current(stale).err(), Some(Refusal::StaleGeneration));
        assert_eq!(rule.removed(stale).err(), Some(Refusal::StaleGeneration));
    }
    assert_eq!(rule.removed(1).map(|owned| owned.generation).ok(), Some(1));
    assert_eq!(rule.removed(1).err(), Some(Refusal::NotInstalled));
    assert_eq!(rule.admit_install(), Ok(()));
    rule.installed(2, TABLE);
    // The previous generation can neither renew nor remove its successor.
    assert_eq!(rule.current(1).err(), Some(Refusal::StaleGeneration));
    assert_eq!(rule.removed(1).err(), Some(Refusal::StaleGeneration));
    assert_eq!(rule.active.as_ref().map(|owned| owned.generation), Some(2));
    assert_eq!(
        rule.current(2).map(|owned| owned.table.clone()).ok(),
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
