//! Local selection criteria, never identity or desktop authority.
use crate::Error;
use std::{fmt, net::IpAddr};

/// A local selection, not a URL, DNS lookup, network scan, or authorization.
#[derive(Clone, Copy)]
pub enum PeerSelector<'a> {
    StableId(&'a str),
    /// Compare both fields against the same fresh daemon record. A saved name
    /// never supplies an address or permission.
    Pinned {
        stable_id: &'a str,
        certificate_name: &'a str,
    },
    /// Compare the ID, canonical name and COMPLETE node-owned address set
    /// against the same fresh daemon record. Addresses constrain selection;
    /// they never replace installed metadata or the server's TLS identity.
    /// A partial set refuses rather than permitting a dial to an unlisted
    /// alternate-family address. Order is immaterial; duplicates are invalid.
    PinnedAddresses {
        stable_id: &'a str,
        certificate_name: &'a str,
        addresses: &'a [IpAddr],
    },
    Name(&'a str),
    Address(IpAddr),
}
impl fmt::Debug for PeerSelector<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PeerSelector([redacted])")
    }
}
impl PeerSelector<'_> {
    pub(crate) fn validate(self) -> Result<(), Error> {
        match self {
            Self::Pinned {
                stable_id,
                certificate_name,
            } => {
                Self::StableId(stable_id).validate()?;
                Self::Name(certificate_name).validate()
            }
            Self::PinnedAddresses {
                stable_id,
                certificate_name,
                addresses,
            } => {
                Self::Pinned {
                    stable_id,
                    certificate_name,
                }
                .validate()?;
                if addresses.is_empty() || addresses.len() > 8 {
                    return Err(Error::InvalidEndpoint);
                }
                for (index, address) in addresses.iter().enumerate() {
                    Self::Address(*address).validate()?;
                    if addresses[..index].contains(address) {
                        return Err(Error::InvalidEndpoint);
                    }
                }
                Ok(())
            }
            Self::StableId(s)
                if !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control) =>
            {
                Ok(())
            }
            Self::Name(s)
                if !s.is_empty()
                    && s.len() <= 254
                    && s.is_ascii()
                    && !s
                        .bytes()
                        .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
                    && !s.bytes().any(|b| b"/:?#@\\".contains(&b)) =>
            {
                Ok(())
            }
            Self::Address(ip)
                if !ip.is_unspecified()
                    && !ip.is_loopback()
                    && !ip.is_multicast()
                    && !matches!(ip, IpAddr::V6(v) if v.to_ipv4_mapped().is_some()) =>
            {
                Ok(())
            }
            _ => Err(Error::InvalidEndpoint),
        }
    }
}
