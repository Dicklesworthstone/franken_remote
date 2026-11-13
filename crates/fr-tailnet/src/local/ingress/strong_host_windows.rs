//! Windows ingress: the strong host model.
//!
//! Windows delivers a packet only when its destination address belongs to
//! the interface it arrived on, unless that arrival interface has weak host
//! receive or forwarding enabled. With both off on every interface of a
//! family (the default), a socket bound to the tailnet address on
//! Tailscale's Wintun interface receives only datagrams that came through
//! the tunnel. As on macOS nothing is installed: the boundary is this kernel
//! state, read through IP Helper (`fr_winsys::net`, synchronous calls that
//! return at once) and rechecked on every refresh. The rules applied to
//! what is read are in `windows_rules.rs`. Seen on oldsurface (Windows 11
//! 26200, 2026-11-12): both settings off on all six interfaces and both
//! families.
//!
//! The same signatures as macOS's `strong_host`, so callers do not
//! distinguish the two. `limit` is the time left to the caller (a periodic
//! refresh passes the old snapshot's remaining validity); a read with none
//! left is refused, as a timed-out macOS command is.
use super::{
    Error,
    windows_rules::{Facts, holder, strong, tunnel},
};
use asupersync::cx::Cx;
use fr_winsys::net;
use std::{
    future::{Ready, ready},
    net::IpAddr,
    time::Duration,
};

/// Whether the kernel drops this family's packets that arrive on an
/// interface other than the one owning their destination address.
pub fn enforced(cx: &Cx, ipv6: bool) -> Ready<Result<bool, Error>> {
    enforced_within(cx, ipv6, super::COMMAND_TIMEOUT)
}

/// [`enforced`] with `limit` left to the caller.
pub(super) fn enforced_within(_cx: &Cx, ipv6: bool, limit: Duration) -> Ready<Result<bool, Error>> {
    if limit.is_zero() {
        return ready(Err(Error::CommandUncertain));
    }
    ready(
        net::ip_interfaces(ipv6)
            .map(|rows| {
                strong(
                    rows.iter()
                        .map(|row| (row.forwarding, row.weak_host_receive)),
                )
            })
            .map_err(|_| Error::CommandFailed),
    )
}

/// The interface's index when it is the up Wintun tunnel named `interface`
/// that alone holds `address` in use.
pub(super) fn qualified(
    _cx: &Cx,
    interface: &str,
    address: IpAddr,
    limit: Duration,
) -> Ready<Result<u32, Error>> {
    if limit.is_zero() {
        return ready(Err(Error::CommandUncertain));
    }
    ready(tunnel_holding(address).and_then(|(index, alias)| {
        if alias == interface {
            Ok(index)
        } else {
            Err(Error::UnqualifiedInterface)
        }
    }))
}

/// The name of the tunnel interface that holds `address`: Tailscale's.
pub fn tunnel_interface(_cx: &Cx, address: IpAddr) -> Ready<Result<String, Error>> {
    ready(tunnel_holding(address).map(|(_, alias)| alias))
}

fn tunnel_holding(address: IpAddr) -> Result<(u32, String), Error> {
    let rows = net::unicast_addresses().map_err(|_| Error::UnqualifiedInterface)?;
    let index = holder(
        rows.iter()
            .map(|row| (row.address, row.interface, row.preferred)),
        address,
    )?;
    let row = net::interface(index).map_err(|_| Error::UnqualifiedInterface)?;
    let facts = Facts {
        alias: &row.alias,
        if_type: row.if_type,
        ip_medium: row.ip_medium,
        hardware_address: row.hardware_address,
        up: row.up,
    };
    if row.index != index || !tunnel(&facts) {
        return Err(Error::UnqualifiedInterface);
    }
    Ok((index, row.alias))
}
