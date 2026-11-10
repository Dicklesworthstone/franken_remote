//! Name/pin policy, and real loopback QUIC/TLS handshakes against an
//! ephemeral CA: the exact name and CA are required; nothing else connects.
use super::*;
use asupersync::{
    net::quic_core::{ConnectionId, LongPacketType, ProtectedHeaderPrefix},
    net::quic_native::handshake_driver::server_config,
    runtime::RuntimeBuilder,
    time::{sleep, timeout},
};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

const NAME: &str = "mac.tailtest.ts.net";

#[test]
fn host_names_are_exact_magicdns_machine_names() {
    for good in [
        "mac-studio.tail1f21e.ts.net",
        "a.b.ts.net",
        "x1.tail-net-9.ts.net",
        "MAC-Studio.Tail1F21E.TS.NET",
    ] {
        let name = HostName::parse(good).unwrap();
        assert_eq!(name.as_str(), good.to_ascii_lowercase());
    }
    let long = format!("{}.tail.ts.net", "a".repeat(64));
    for bad in [
        "",
        "ts.net",
        "tail.ts.net",
        "a.b.c.ts.net",
        "mac.tail1f21e.ts.net.",
        "mac.tail1f21e.ts.com",
        "mac.tail1f21e.example.net",
        "100.89.198.71",
        "[fd7a:115c:a1e0::1]",
        "mac..ts.net",
        "-mac.tail.ts.net",
        "mac-.tail.ts.net",
        "mac.-tail.ts.net",
        "m_c.tail.ts.net",
        "*.tail.ts.net",
        "mac .tail.ts.net",
        " mac.tail.ts.net",
        "mäc.tail.ts.net",
        "mac.tail.ts.net:8443",
        long.as_str(),
    ] {
        assert_eq!(
            HostName::parse(bad).err(),
            Some(Error::InvalidEndpoint),
            "{bad:?}"
        );
    }
    // Exactly 63 is a label; 64 is not (above).
    HostName::parse(&format!("{}.tail.ts.net", "a".repeat(63))).unwrap();
}

#[test]
fn only_tailscale_ranges_count_as_tailnet_addresses() {
    for inside in [
        "100.64.0.0",
        "100.89.198.71",
        "100.127.255.255",
        "fd7a:115c:a1e0::1",
        "fd7a:115c:a1e0:ffff:ffff:ffff:ffff:ffff",
        // Next to the reserved blocks, still node space.
        "100.100.99.255",
        "100.100.101.0",
        "100.115.91.255",
        "100.115.94.0",
        "fd7a:115c:a1e0:b19::1",
        "fd7a:115c:a1e0:b1b::1",
    ] {
        assert!(is_tailnet_address(inside.parse().unwrap()), "{inside}");
    }
    for outside in [
        "100.63.255.255",
        "100.128.0.0",
        "10.0.0.1",
        "127.0.0.1",
        "8.8.8.8",
        "fd7a:115c:a1e1::1",
        "fd7a:115d:a1e0::1",
        "::1",
        "::ffff:100.64.0.1",
        "2001:db8::1",
        // Tailscale's own service addresses (MagicDNS answers from here).
        "100.100.100.100",
        "100.100.100.0",
        "100.100.100.255",
        // ChromeOS guest range.
        "100.115.92.1",
        "100.115.93.255",
        // 4via6: a host behind a subnet router, not a node.
        "fd7a:115c:a1e0:b1a::a01:110",
        "fd7a:115c:a1e0:b1a:ffff:ffff:ffff:ffff",
    ] {
        assert!(!is_tailnet_address(outside.parse().unwrap()), "{outside}");
    }
}

fn ips(list: &[&str]) -> Vec<IpAddr> {
    list.iter().map(|s| s.parse().unwrap()).collect()
}

#[test]
fn resolver_output_is_tailnet_only_unique_bounded_and_ipv6_first() {
    assert_eq!(
        checked_addresses(ips(&["100.64.0.2", "fd7a:115c:a1e0::2", "100.64.0.2"])).unwrap(),
        ips(&["fd7a:115c:a1e0::2", "100.64.0.2"])
    );
    // One off-tailnet answer refuses the whole resolution, not just itself.
    assert_eq!(
        checked_addresses(ips(&["100.64.0.2", "203.0.113.9"])).err(),
        Some(Error::OutsideTailnet)
    );
    assert_eq!(
        checked_addresses(Vec::new()).err(),
        Some(Error::NameUnresolved)
    );
    assert_eq!(
        checked_addresses(ips(&[
            "100.64.0.1",
            "100.64.0.2",
            "100.64.0.3",
            "100.64.0.4",
            "100.64.0.5"
        ]))
        .err(),
        Some(Error::OutsideTailnet)
    );
}

fn pin(list: &[&str]) -> HostPin {
    HostPin::new(HostName::parse(NAME).unwrap(), ips(list)).unwrap()
}

#[test]
fn pins_round_trip_only_in_canonical_form() {
    let pinned = pin(&["fd7a:115c:a1e0::2", "100.64.0.2"]);
    let text = pinned.to_string();
    assert_eq!(
        text,
        "fr-pin-v1 mac.tailtest.ts.net 100.64.0.2 fd7a:115c:a1e0::2"
    );
    assert_eq!(text.parse::<HostPin>().unwrap(), pinned);
    for bad in [
        "",
        "fr-pin-v1",
        "fr-pin-v1 mac.tailtest.ts.net",
        "fr-pin-v2 mac.tailtest.ts.net 100.64.0.2",
        "fr-pin-v1 MAC.tailtest.ts.net 100.64.0.2",
        "fr-pin-v1 mac.tailtest.ts.net fd7a:115c:a1e0::2 100.64.0.2",
        "fr-pin-v1 mac.tailtest.ts.net 100.64.0.2 100.64.0.2",
        "fr-pin-v1 mac.tailtest.ts.net  100.64.0.2",
        "fr-pin-v1 mac.tailtest.ts.net 100.64.0.2 ",
        "fr-pin-v1 mac.tailtest.ts.net 192.0.2.1",
        "fr-pin-v1 mac.tailtest.ts.net 100.64.0.2 junk",
        "fr-pin-v1 100.64.0.2 100.64.0.2",
    ] {
        assert_eq!(
            bad.parse::<HostPin>().err(),
            Some(Error::InvalidPolicy),
            "{bad:?}"
        );
    }
}

#[test]
fn a_pin_admits_only_its_own_name_and_already_pinned_addresses() {
    let pinned = pin(&["100.64.0.2", "fd7a:115c:a1e0::2"]);
    let name = HostName::parse(NAME).unwrap();
    pinned
        .admits(&name, &ips(&["fd7a:115c:a1e0::2", "100.64.0.2"]))
        .unwrap();
    pinned.admits(&name, &ips(&["100.64.0.2"])).unwrap();
    // Planted negative: the name now resolves to another node's address.
    assert_eq!(
        pinned.admits(&name, &ips(&["100.64.0.3"])).err(),
        Some(Error::IdentityChanged)
    );
    assert_eq!(
        pinned
            .admits(&name, &ips(&["100.64.0.2", "fd7a:115c:a1e0::3"]))
            .err(),
        Some(Error::IdentityChanged)
    );
    assert_eq!(
        pinned.admits(&name, &[]).err(),
        Some(Error::IdentityChanged)
    );
    let other = HostName::parse("other.tailtest.ts.net").unwrap();
    assert_eq!(
        pinned.admits(&other, &ips(&["100.64.0.2"])).err(),
        Some(Error::InvalidPolicy)
    );
}

#[test]
fn a_family_the_pin_never_saw_is_admitted_and_recorded_but_a_pinned_one_must_match() {
    let name = HostName::parse(NAME).unwrap();
    // MagicDNS answered A only when this pin was made.
    let v4_only = pin(&["100.64.0.2"]);
    let both = ips(&["fd7a:115c:a1e0::2", "100.64.0.2"]);
    let observed = v4_only.admits(&name, &both).unwrap();
    assert_eq!(
        observed.addresses(),
        ips(&["100.64.0.2", "fd7a:115c:a1e0::2"])
    );
    // An unchanged answer records the pin as it was.
    assert_eq!(
        v4_only.admits(&name, &ips(&["100.64.0.2"])).unwrap(),
        v4_only
    );
    // The pinned family still decides: a new IPv4 address is a new node even
    // when an IPv6 address arrives alongside it.
    assert_eq!(
        v4_only
            .admits(&name, &ips(&["fd7a:115c:a1e0::2", "100.64.0.3"]))
            .err(),
        Some(Error::IdentityChanged)
    );
    // An IPv6-only pin likewise admits a first IPv4 answer.
    let v6_only = pin(&["fd7a:115c:a1e0::2"]);
    assert_eq!(
        v6_only.admits(&name, &both).unwrap().addresses(),
        ips(&["100.64.0.2", "fd7a:115c:a1e0::2"])
    );
    assert_eq!(
        v6_only.admits(&name, &ips(&["fd7a:115c:a1e0::9"])).err(),
        Some(Error::IdentityChanged)
    );
}

fn openssl(dir: &Path, args: &[&str]) {
    let output = Command::new("openssl")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture OpenSSL: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Two ephemeral CAs: `ca` signs leaves for the exact name and for another
/// name; `rogue` signs a leaf for the exact name that no client trusts.
fn pki() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        use std::os::unix::fs::DirBuilderExt;
        let dir = std::env::temp_dir().join(format!(
            "fr-named-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        // `req` would also apply the system openssl.cnf. macOS LibreSSL's sets
        // x509_extensions = v3_ca, which adds a second basicConstraints next to
        // -addext; WebPKI rejects the CA (ExtensionValueInvalid). Use only ours.
        std::fs::write(dir.join("req.cnf"), "[req]\ndistinguished_name=dn\n[dn]\n").unwrap();
        for ca in ["ca", "rogue"] {
            issue_ca(&dir, ca);
        }
        for (label, ca, name) in [
            ("leaf", "ca", NAME),
            ("other", "ca", "other.tailtest.ts.net"),
            ("forged", "rogue", NAME),
        ] {
            issue_leaf(&dir, label, ca, name);
        }
        dir
    })
}
/// A self-signed P-256 CA (`<ca>.pem`, `<ca>.key`) valid for two days.
fn issue_ca(dir: &Path, ca: &str) {
    openssl(
        dir,
        &[
            "req",
            "-config",
            "req.cnf",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-pkeyopt",
            "ec_param_enc:named_curve",
            "-nodes",
            "-keyout",
            &format!("{ca}.key"),
            "-out",
            &format!("{ca}.pem"),
            "-days",
            "2",
            "-subj",
            &format!("/CN=Ephemeral named test {ca}"),
            "-addext",
            "basicConstraints=critical,CA:TRUE",
        ],
    );
}
/// A P-256 server leaf for `name` signed by `ca`, also as DER (`<label>.der`,
/// `<label>.key.der`).
fn issue_leaf(dir: &Path, label: &str, ca: &str, name: &str) {
    openssl(
        dir,
        &[
            "req",
            "-config",
            "req.cnf",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-pkeyopt",
            "ec_param_enc:named_curve",
            "-nodes",
            "-keyout",
            &format!("{label}.key"),
            "-out",
            &format!("{label}.csr"),
            "-subj",
            "/CN=Ephemeral named leaf",
        ],
    );
    std::fs::write(
        dir.join(format!("{label}.ext")),
        format!(
            "subjectAltName=DNS:{name}\nbasicConstraints=critical,CA:FALSE\n\
             keyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n"
        ),
    )
    .unwrap();
    openssl(
        dir,
        &[
            "x509",
            "-req",
            "-sha256",
            "-in",
            &format!("{label}.csr"),
            "-CA",
            &format!("{ca}.pem"),
            "-CAkey",
            &format!("{ca}.key"),
            "-CAcreateserial",
            "-days",
            "2",
            "-extfile",
            &format!("{label}.ext"),
            "-out",
            &format!("{label}.pem"),
        ],
    );
    openssl(
        dir,
        &[
            "x509",
            "-in",
            &format!("{label}.pem"),
            "-outform",
            "DER",
            "-out",
            &format!("{label}.der"),
        ],
    );
    openssl(
        dir,
        &[
            "pkcs8",
            "-topk8",
            "-nocrypt",
            "-in",
            &format!("{label}.key"),
            "-outform",
            "DER",
            "-out",
            &format!("{label}.key.der"),
        ],
    );
}
fn roots(ca: &str) -> Vec<Certificate> {
    Certificate::from_pem(&std::fs::read(pki().join(format!("{ca}.pem"))).unwrap()).unwrap()
}
fn runtime() -> asupersync::runtime::Runtime {
    RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
fn initial_destination(data: &[u8]) -> Result<ConnectionId, ()> {
    let ProtectedHeaderPrefix::Long(header) =
        ProtectedHeaderPrefix::decode(data, 0).map_err(|_| ())?
    else {
        return Err(());
    };
    if header.packet_type != LongPacketType::Initial || header.version != 1 {
        return Err(());
    }
    Ok(header.dst_cid)
}
async fn accept(
    cx: &Cx,
    mut endpoint: QuicUdpEndpoint,
    leaf: &str,
) -> Result<NativeQuicUdpConnection, ()> {
    let packets = endpoint.receive_batch(cx, 1).await.map_err(|_| ())?;
    let destination = initial_destination(&packets[0].data)?;
    let config = server_config(
        vec![
            std::fs::read(pki().join(format!("{leaf}.der")))
                .unwrap()
                .into(),
        ],
        std::fs::read(pki().join(format!("{leaf}.key.der")))
            .unwrap()
            .try_into()
            .unwrap(),
        vec![ALPN.to_vec()],
    )
    .unwrap();
    let driver = QuicHandshakeDriver::server(config, transport_parameters().unwrap()).unwrap();
    NativeQuicUdpConnection::accept(
        cx,
        endpoint,
        driver,
        destination,
        ConnectionId::new(b"named-test-server").unwrap(),
        connection_config(),
        ALPN,
    )
    .await
    .map_err(|_| ())
}

/// Dial the loopback server through the named dialer's own resolved-address
/// path (resolution and the tailnet-range check are covered above).
fn handshake(leaf: &str, ca: &str) -> Result<NativeQuicUdpConnection, Error> {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let client = NamedClient::new(
            HostName::parse(NAME).unwrap(),
            roots(ca),
            Duration::from_secs(4),
        )
        .unwrap();
        let endpoint =
            QuicUdpEndpoint::bind(&cx, "127.0.0.1:0".parse().unwrap(), endpoint_config())
                .await
                .unwrap();
        let port = endpoint.local_addr().port();
        let started = now(&cx).unwrap();
        let end = started + 4_000_000;
        let loopback = ips(&["127.0.0.1"]);
        let mut dial = Box::pin(client.dial_resolved(&cx, &loopback, port, started, end));
        let mut host = Box::pin(accept(&cx, endpoint, leaf));
        let (mut dialed, mut accepted) = (None, None);
        let both = std::future::poll_fn(|task| {
            if dialed.is_none()
                && let Poll::Ready(v) = dial.as_mut().poll(task)
            {
                dialed = Some(v);
            }
            if accepted.is_none()
                && let Poll::Ready(v) = host.as_mut().poll(task)
            {
                accepted = Some(v);
            }
            // A refused client ends the test; the server may still wait.
            match &dialed {
                Some(Err(_)) => Poll::Ready(()),
                Some(Ok(_)) if accepted.is_some() => Poll::Ready(()),
                _ => Poll::Pending,
            }
        });
        Box::pin(timeout(cx.now(), Duration::from_secs(6), both))
            .await
            .expect("handshake fixture finished");
        let result = dialed.unwrap();
        if let Ok(native) = &result {
            let host = accepted.unwrap().unwrap();
            assert_eq!(native.peer_addr(), host.local_addr());
            assert_eq!(native.negotiated_alpn(), ALPN);
        }
        result
    })
}

#[test]
fn the_exact_name_under_a_trusted_root_completes_a_real_handshake() {
    let native = handshake("leaf", "ca").unwrap();
    assert!(native.connection().can_send_app_data());
    assert!(
        native.connection().inner().streams().is_empty(),
        "dialing sends no application data"
    );
}

// The pinned Asupersync reports the client's own certificate refusal as a
// generic fatal handshake alert (`read_hs_fatal_alert`), which
// `profile::handshake_error` maps to `NativeHandshake`. The distinct
// `PeerCertificateRejected` needs the unlanded upstream certificate-code
// change; these tests pin today's exact value, and the positive control above
// (same fixture, right name and root) proves the refusal is the certificate.
const CERTIFICATE_REFUSAL: Error = Error::NativeHandshake;

#[test]
fn another_name_under_the_trusted_root_is_refused() {
    assert_eq!(handshake("other", "ca").err(), Some(CERTIFICATE_REFUSAL));
}

#[test]
fn the_exact_name_under_an_untrusted_root_is_refused() {
    assert_eq!(handshake("forged", "ca").err(), Some(CERTIFICATE_REFUSAL));
    // And the forged chain's own root does not vouch for the real leaf.
    assert_eq!(handshake("leaf", "rogue").err(), Some(CERTIFICATE_REFUSAL));
}

#[test]
fn dial_refuses_a_foreign_pin_port_zero_and_a_second_concurrent_dial() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let client = NamedClient::new(
            HostName::parse(NAME).unwrap(),
            roots("ca"),
            Duration::from_secs(1),
        )
        .unwrap();
        let foreign = HostPin::new(
            HostName::parse("other.tailtest.ts.net").unwrap(),
            ips(&["100.64.0.2"]),
        )
        .unwrap();
        assert_eq!(
            client.dial(&cx, 8443, Some(&foreign)).err(),
            Some(Error::InvalidPolicy)
        );
        assert_eq!(
            client.dial(&cx, 0, None).err(),
            Some(Error::InvalidEndpoint)
        );
        // The slot is claimed at call time and released on drop, even unpolled.
        let first = client.dial(&cx, 8443, None).unwrap();
        assert_eq!(client.dial(&cx, 8443, None).err(), Some(Error::Busy));
        drop(first);
        drop(client.dial(&cx, 8443, None).unwrap());
        sleep(cx.now(), Duration::from_millis(1)).await;
    });
}
