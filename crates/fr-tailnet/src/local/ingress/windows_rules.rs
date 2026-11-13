//! What Windows interface facts mean for ingress (`strong_host_windows.rs`
//! reads them). Plain data in, so the rules are tested on every platform.
use super::{Error, interface_valid};
use std::net::IpAddr;

/// Wintun's interface type (`IF_TYPE_PROP_VIRTUAL`).
const PROP_VIRTUAL: u32 = 53;

/// One interface's settings for a family: forwarding, weak host receive.
pub(super) type IpSettings = (bool, bool);

/// The strong host model holds for a family when no interface forwards and
/// none accepts packets addressed to another interface. No interfaces at
/// all is no evidence.
pub(super) fn strong(settings: impl IntoIterator<Item = IpSettings>) -> bool {
    let mut seen = false;
    for (forwarding, weak_host_receive) in settings {
        if forwarding || weak_host_receive {
            return false;
        }
        seen = true;
    }
    seen
}

/// One unicast address: the address, its interface, DAD passed.
pub(super) type Held = (IpAddr, u32, bool);

/// The one interface holding `address` in use. An address on two
/// interfaces is not one boundary; a tentative or duplicate one is not in
/// use.
pub(super) fn holder(rows: impl IntoIterator<Item = Held>, address: IpAddr) -> Result<u32, Error> {
    let mut found = None;
    for (held, interface, preferred) in rows {
        if held != address {
            continue;
        }
        if !preferred || found.is_some_and(|index| index != interface) {
            return Err(Error::UnqualifiedInterface);
        }
        found = Some(interface);
    }
    found.ok_or(Error::UnqualifiedInterface)
}

/// An interface's identity, from `MIB_IF_ROW2`.
pub(super) struct Facts<'a> {
    pub(super) alias: &'a str,
    pub(super) if_type: u32,
    pub(super) ip_medium: bool,
    pub(super) hardware_address: bool,
    pub(super) up: bool,
}

/// A layer-3 tunnel like Linux's `IFF_TUN` or macOS's utun: virtual, IP
/// packets with no link layer, up, and named so the configuration can name
/// it ([`interface_valid`]).
pub(super) fn tunnel(facts: &Facts<'_>) -> bool {
    facts.up
        && facts.if_type == PROP_VIRTUAL
        && facts.ip_medium
        && !facts.hardware_address
        && interface_valid(facts.alias)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_family_is_strong_only_when_no_interface_forwards_or_receives_weakly() {
        assert!(strong([(false, false), (false, false)]));
        assert!(!strong([(false, false), (true, false)]));
        assert!(!strong([(false, true), (false, false)]));
        assert!(!strong([]));
    }

    #[test]
    fn an_address_is_held_by_exactly_one_interface_in_use() {
        let tailnet: IpAddr = "100.64.0.7".parse().unwrap();
        let other: IpAddr = "192.168.1.9".parse().unwrap();
        assert_eq!(
            holder([(other, 16, true), (tailnet, 26, true)], tailnet),
            Ok(26)
        );
        assert_eq!(
            holder([(tailnet, 26, true), (tailnet, 26, true)], tailnet),
            Ok(26)
        );
        for rows in [
            vec![(other, 16, true)],
            vec![(tailnet, 26, false)],
            vec![(tailnet, 26, true), (tailnet, 16, true)],
            vec![(tailnet, 26, true), (tailnet, 26, false)],
        ] {
            assert_eq!(
                holder(rows.clone(), tailnet),
                Err(Error::UnqualifiedInterface),
                "{rows:?}"
            );
        }
    }

    #[test]
    fn only_an_up_wintun_like_layer3_interface_is_a_tunnel() {
        let wintun = Facts {
            alias: "Tailscale",
            if_type: 53,
            ip_medium: true,
            hardware_address: false,
            up: true,
        };
        assert!(tunnel(&wintun));
        let refused = [
            Facts {
                up: false,
                ..wintun
            },
            // Ethernet-like (a Wi-Fi or TAP adapter).
            Facts {
                if_type: 6,
                ..wintun
            },
            Facts {
                ip_medium: false,
                ..wintun
            },
            Facts {
                hardware_address: true,
                ..wintun
            },
            // A name the configuration could not carry.
            Facts {
                alias: "Tailscale 2",
                ..wintun
            },
        ];
        for facts in &refused {
            assert!(!tunnel(facts), "{}", facts.alias);
        }
    }
}
