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
