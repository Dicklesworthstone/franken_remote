//! The kernel's IP interfaces and addresses (IP Helper), as plain facts:
//! fr-tailnet decides what they mean for ingress.
//!
//! Each read is one synchronous call into the network stack that returns at
//! once (no I/O wait), so async callers may make it inline.
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use windows::Win32::Foundation::{NO_ERROR, WIN32_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfEntry2, GetIpInterfaceTable, GetUnicastIpAddressTable, MIB_IF_ROW2,
    MIB_IPINTERFACE_ROW, MIB_IPINTERFACE_TABLE, MIB_UNICASTIPADDRESS_ROW,
    MIB_UNICASTIPADDRESS_TABLE,
};
use windows::Win32::NetworkManagement::Ndis::{IfOperStatusUp, NdisMediumIP};
use windows::Win32::Networking::WinSock::{
    ADDRESS_FAMILY, AF_INET, AF_INET6, AF_UNSPEC, IpDadStatePreferred,
};

/// One interface's IP settings for a family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpInterface {
    pub index: u32,
    /// Forwards packets between interfaces.
    pub forwarding: bool,
    /// Accepts packets addressed to another interface's addresses.
    pub weak_host_receive: bool,
}

/// One unicast address and the interface that holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnicastAddress {
    pub address: IpAddr,
    pub interface: u32,
    /// Duplicate address detection passed: the address is in use.
    pub preferred: bool,
}

/// What an interface is, from its `MIB_IF_ROW2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub index: u32,
    /// The administrator-visible name (`Get-NetAdapter`'s Name).
    pub alias: String,
    pub description: String,
    /// IANA ifType (53 is `IF_TYPE_PROP_VIRTUAL`).
    pub if_type: u32,
    /// The medium carries IP packets, not link-layer frames.
    pub ip_medium: bool,
    /// The interface has a link-layer (MAC) address.
    pub hardware_address: bool,
    /// Operationally up.
    pub up: bool,
}

fn win32(code: WIN32_ERROR) -> io::Result<()> {
    if code == NO_ERROR {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code.0.cast_signed()))
    }
}

fn family(ipv6: bool) -> ADDRESS_FAMILY {
    if ipv6 { AF_INET6 } else { AF_INET }
}

/// Every interface's settings for IPv4 or IPv6.
pub fn ip_interfaces(ipv6: bool) -> io::Result<Vec<IpInterface>> {
    let mut table: *mut MIB_IPINTERFACE_TABLE = core::ptr::null_mut();
    // SAFETY: an out pointer to a local; the table is freed below.
    win32(unsafe { GetIpInterfaceTable(family(ipv6), &raw mut table) })?;
    // SAFETY: on success `table` is a valid table of `NumEntries` rows,
    // read here and freed once.
    let rows: Vec<IpInterface> = unsafe {
        let count = (*table).NumEntries as usize;
        let first: *const MIB_IPINTERFACE_ROW = (&raw const (*table).Table).cast();
        let rows = core::slice::from_raw_parts(first, count)
            .iter()
            .map(|row| IpInterface {
                index: row.InterfaceIndex,
                forwarding: row.ForwardingEnabled,
                weak_host_receive: row.WeakHostReceive,
            })
            .collect();
        FreeMibTable(table.cast());
        rows
    };
    Ok(rows)
}

/// Every unicast address of both families.
pub fn unicast_addresses() -> io::Result<Vec<UnicastAddress>> {
    let mut table: *mut MIB_UNICASTIPADDRESS_TABLE = core::ptr::null_mut();
    // SAFETY: an out pointer to a local; the table is freed below.
    win32(unsafe { GetUnicastIpAddressTable(AF_UNSPEC, &raw mut table) })?;
    // SAFETY: on success `table` is a valid table of `NumEntries` rows,
    // read here and freed once. Each row's address union is read through
    // the member its family names.
    let rows: Vec<UnicastAddress> = unsafe {
        let count = (*table).NumEntries as usize;
        let first: *const MIB_UNICASTIPADDRESS_ROW = (&raw const (*table).Table).cast();
        let rows = core::slice::from_raw_parts(first, count)
            .iter()
            .filter_map(|row| {
                let address = match row.Address.si_family {
                    AF_INET => IpAddr::V4(Ipv4Addr::from(u32::from_be(
                        row.Address.Ipv4.sin_addr.S_un.S_addr,
                    ))),
                    AF_INET6 => IpAddr::V6(Ipv6Addr::from(row.Address.Ipv6.sin6_addr.u.Byte)),
                    _ => return None,
                };
                Some(UnicastAddress {
                    address,
                    interface: row.InterfaceIndex,
                    preferred: row.DadState == IpDadStatePreferred,
                })
            })
            .collect();
        FreeMibTable(table.cast());
        rows
    };
    Ok(rows)
}

fn text(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// The interface with this index.
pub fn interface(index: u32) -> io::Result<Interface> {
    let mut row = MIB_IF_ROW2 {
        InterfaceIndex: index,
        ..Default::default()
    };
    // SAFETY: a row on the stack naming the interface by index.
    win32(unsafe { GetIfEntry2(&raw mut row) })?;
    Ok(Interface {
        index: row.InterfaceIndex,
        alias: text(&row.Alias),
        description: text(&row.Description),
        if_type: row.Type,
        ip_medium: row.MediaType == NdisMediumIP,
        hardware_address: row.PhysicalAddressLength != 0,
        up: row.OperStatus == IfOperStatusUp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loopback holds 127.0.0.1 and `::1`,
    /// is up, and has no hardware address; these reads see it.
    #[test]
    fn the_loopback_interface_is_read() {
        let addresses = unicast_addresses().expect("addresses");
        let loopback = addresses
            .iter()
            .find(|a| a.address == IpAddr::V4(Ipv4Addr::LOCALHOST))
            .expect("127.0.0.1");
        assert!(loopback.preferred);
        assert!(
            addresses
                .iter()
                .any(|a| a.address == IpAddr::V6(Ipv6Addr::LOCALHOST)
                    && a.interface == loopback.interface)
        );
        let interface = interface(loopback.interface).expect("interface");
        assert_eq!(interface.index, loopback.interface);
        assert!(interface.up);
        assert!(!interface.hardware_address);
        assert_eq!(interface.if_type, 24); // IF_TYPE_SOFTWARE_LOOPBACK
        for ipv6 in [false, true] {
            assert!(
                ip_interfaces(ipv6)
                    .expect("ip interfaces")
                    .iter()
                    .any(|row| row.index == loopback.interface)
            );
        }
        assert!(super::interface(u32::MAX).is_err());
    }
}
