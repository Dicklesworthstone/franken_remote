//! macOS ingress: the kernel's strong destination-address check.
//!
//! With `check_interface` = 1 and forwarding off for a family, XNU drops every
//! packet whose destination address is owned by another interface than the one
//! it arrived on. A socket bound to the tailnet address on Tailscale's utun then
//! receives only datagrams that came through the tunnel. Nothing is installed:
//! the boundary is this kernel state, read root-free and rechecked on every
//! refresh. Measured on macOS 27.0.1 (2026-11-08) with raw frames from a LAN
//! host addressed to the Mac's tailnet addresses: IPv6 (check on, forwarding
//! off) dropped them; IPv4 with forwarding on delivered them, to sockets with
//! and without `IP_BOUND_IF`, which therefore is no ingress restriction.
use super::{COMMAND_TIMEOUT, Error, command, command_within, interface_valid};
use asupersync::cx::Cx;
use std::{net::IpAddr, path::Path, time::Duration};

const SYSCTL: &str = "/usr/sbin/sysctl";
const IFCONFIG: &str = "/sbin/ifconfig";

/// Whether the kernel drops this family's packets that arrive on an interface
/// other than the one owning their destination address.
pub async fn enforced(cx: &Cx, ipv6: bool) -> Result<bool, Error> {
    enforced_within(cx, ipv6, COMMAND_TIMEOUT).await
}

/// [`enforced`] with the `sysctl` read bounded by `limit`.
pub(super) async fn enforced_within(cx: &Cx, ipv6: bool, limit: Duration) -> Result<bool, Error> {
    let names: [&str; 2] = if ipv6 {
        ["net.inet6.ip6.check_interface", "net.inet6.ip6.forwarding"]
    } else {
        ["net.inet.ip.check_interface", "net.inet.ip.forwarding"]
    };
    let args = ["-n", names[0], names[1]];
    let bytes = command_within(cx, Path::new(SYSCTL), &args, b"", limit).await?;
    strong_host_values(&bytes)
}

/// `sysctl -n CHECK FORWARDING` prints exactly two decimal values.
fn strong_host_values(bytes: &[u8]) -> Result<bool, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::CommandFailed)?;
    let values: Vec<u32> = text
        .lines()
        .map(|line| line.trim().parse().map_err(|_| Error::CommandFailed))
        .collect::<Result<_, _>>()?;
    match values.as_slice() {
        [check, forwarding] => Ok(*check == 1 && *forwarding == 0),
        _ => Err(Error::CommandFailed),
    }
}

/// The interface's index when it is an up point-to-point utun that holds
/// `address` (not tentative, duplicated or detached).
/// `limit` bounds the `ifconfig` read.
pub(super) async fn qualified(
    cx: &Cx,
    interface: &str,
    address: IpAddr,
    limit: Duration,
) -> Result<u32, Error> {
    if !interface.starts_with("utun") || !interface_valid(interface) {
        return Err(Error::UnqualifiedInterface);
    }
    let bytes = command_within(cx, Path::new(IFCONFIG), &["-v", interface], b"", limit).await?;
    tunnel_index(&bytes, interface, address)
}

fn tunnel_index(bytes: &[u8], interface: &str, address: IpAddr) -> Result<u32, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::UnqualifiedInterface)?;
    let mut lines = text.lines();
    let header = lines.next().ok_or(Error::UnqualifiedInterface)?;
    let rest = header
        .strip_prefix(interface)
        .and_then(|rest| rest.strip_prefix(": flags="))
        .ok_or(Error::UnqualifiedInterface)?;
    let flags = rest
        .split_once('<')
        .and_then(|(_, rest)| rest.split_once('>'))
        .map(|(flags, _)| flags)
        .ok_or(Error::UnqualifiedInterface)?;
    if !["UP", "POINTOPOINT", "RUNNING"]
        .iter()
        .all(|wanted| flags.split(',').any(|flag| flag == *wanted))
    {
        return Err(Error::UnqualifiedInterface);
    }
    let index: u32 = rest
        .split_whitespace()
        .skip_while(|word| *word != "index")
        .nth(1)
        .and_then(|value| value.parse().ok())
        .filter(|&index| index != 0)
        .ok_or(Error::UnqualifiedInterface)?;
    let held = lines.any(|line| {
        let mut words = line.split_whitespace();
        let family = words.next();
        let Some(value) = words.next() else {
            return false;
        };
        let usable = !line
            .split_whitespace()
            .any(|word| matches!(word, "tentative" | "duplicated" | "detached" | "deprecated"));
        let parsed = match family {
            Some("inet") => value.parse::<std::net::Ipv4Addr>().ok().map(IpAddr::V4),
            // A scoped (link-local) address carries `%interface`; never ours.
            Some("inet6") => value.parse::<std::net::Ipv6Addr>().ok().map(IpAddr::V6),
            _ => None,
        };
        usable && parsed == Some(address)
    });
    if !held {
        return Err(Error::UnqualifiedInterface);
    }
    Ok(index)
}

/// The utun interface that holds `address`: Tailscale's tunnel. Exactly one
/// must qualify; an address on two tunnels is not one boundary.
pub async fn tunnel_interface(cx: &Cx, address: IpAddr) -> Result<String, Error> {
    let bytes = command(cx, Path::new(IFCONFIG), &["-l"], b"").await?;
    let names = std::str::from_utf8(&bytes).map_err(|_| Error::UnqualifiedInterface)?;
    let mut found = None;
    for name in names
        .split_whitespace()
        .filter(|name| name.starts_with("utun") && interface_valid(name))
        .take(64)
    {
        if qualified(cx, name, address, COMMAND_TIMEOUT).await.is_ok() {
            if found.is_some() {
                return Err(Error::UnqualifiedInterface);
            }
            found = Some(name.to_owned());
        }
    }
    found.ok_or(Error::UnqualifiedInterface)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTUN: &str = "utun8: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1280 index 40
\teflags=4002080<TXSTART,NOAUTOIPV6LL,CHANNEL_DRV>
\txflags=4010004<NOAUTONX,IS_VPN,INBAND_WAKE_PKT>
\tinet6 fe80::7ed6:2cff:fe0c:c9cf%utun8 prefixlen 64 scopeid 0x28
\tinet 100.89.198.71 --> 100.89.198.71 netmask 0xffffffff
\tinet6 fd7a:115c:a1e0::5a38:c648 prefixlen 48
";

    #[test]
    fn sysctl_values_enforce_only_with_the_check_on_and_forwarding_off() {
        assert_eq!(strong_host_values(b"1\n0\n"), Ok(true));
        assert_eq!(strong_host_values(b"1\n1\n"), Ok(false));
        assert_eq!(strong_host_values(b"0\n0\n"), Ok(false));
        for malformed in [&b""[..], b"1\n", b"1\n0\n0\n", b"one\n0\n", b"1 0\n"] {
            assert!(strong_host_values(malformed).is_err(), "{malformed:?}");
        }
    }

    #[test]
    fn a_periodic_strong_host_read_may_use_the_old_snapshot_validity() {
        // A periodic refresh passes the old snapshot's remaining validity to
        // both kernel reads; installation keeps one second.
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .worker_threads(2)
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let cx = Cx::current().unwrap();
            let long = Duration::from_millis(2_500);
            let address: IpAddr = "fd7a:115c:a1e0::1".parse().unwrap();
            assert_eq!(
                enforced_within(&cx, true, Duration::ZERO).await,
                Err(Error::CommandUncertain)
            );
            assert!(enforced_within(&cx, true, long).await.is_ok());
            assert_eq!(
                qualified(&cx, "utun999", address, Duration::ZERO).await,
                Err(Error::CommandUncertain)
            );
            assert_eq!(
                qualified(&cx, "utun999", address, long).await,
                Err(Error::CommandFailed)
            );
            // A real child that outlives one second completes within the
            // longer limit.
            let sleep = Path::new("/bin/sleep");
            assert_eq!(
                command_within(&cx, sleep, &["1.3"], b"", COMMAND_TIMEOUT).await,
                Err(Error::CommandUncertain)
            );
            assert_eq!(
                command_within(&cx, sleep, &["1.3"], b"", long).await,
                Ok(Vec::new())
            );
        });
    }

    #[test]
    fn a_tunnel_qualifies_only_with_its_own_usable_address() {
        let v4: IpAddr = "100.89.198.71".parse().unwrap();
        let v6: IpAddr = "fd7a:115c:a1e0::5a38:c648".parse().unwrap();
        assert_eq!(tunnel_index(UTUN.as_bytes(), "utun8", v4), Ok(40));
        assert_eq!(tunnel_index(UTUN.as_bytes(), "utun8", v6), Ok(40));
        let other: IpAddr = "100.89.198.72".parse().unwrap();
        let link_local: IpAddr = "fe80::7ed6:2cff:fe0c:c9cf".parse().unwrap();
        for address in [other, link_local] {
            assert!(tunnel_index(UTUN.as_bytes(), "utun8", address).is_err());
        }
        // Another interface's report, a down or broadcast interface, a missing
        // index and a tentative address are all refused.
        assert!(tunnel_index(UTUN.as_bytes(), "utun9", v4).is_err());
        let down = UTUN.replace("<UP,POINTOPOINT,", "<POINTOPOINT,");
        let broadcast = UTUN.replace("POINTOPOINT", "BROADCAST");
        let unindexed = UTUN.replace(" index 40", "");
        let tentative = UTUN.replace("prefixlen 48", "prefixlen 48 tentative");
        assert_ne!(tentative, UTUN);
        for (report, address) in [
            (&down, v4),
            (&broadcast, v4),
            (&unindexed, v4),
            (&tentative, v6),
        ] {
            assert!(
                tunnel_index(report.as_bytes(), "utun8", address).is_err(),
                "{report}"
            );
        }
    }
}
