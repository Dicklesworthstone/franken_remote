//! Authenticated installed-application process boundary, without session policy.
//!
//! Runs the installed Tailscale application's signed CLI, never a caller's
//! executable, shell or HTTP proxy. Only fixed status and numeric-address
//! `WhoIs` queries exist. `fr-tailnet` no longer uses it: since 2026-11-08 it
//! reads the app's loopback `LocalAPI`, which this CLI itself connects to. Callers must own every child on a drained
//! blocking operation and check their original deadline before resuming it.
//! This crate neither parses metadata nor grants observation or input authority.
use std::{fmt, net::IpAddr};

/// The complete command vocabulary; there is no arbitrary argv or path API.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Status,
    WhoIs(IpAddr),
}
impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Status => "Status",
            Self::WhoIs(_) => "WhoIs([redacted])",
        })
    }
}
impl Command {
    /// The native boundary receives a command tag and address bytes, never
    /// untrusted argument text. C formats that numeric address itself.
    fn wire(self) -> Result<(u8, [u8; 16]), Error> {
        let mut bytes = [0; 16];
        match self {
            Self::Status => Ok((0, bytes)),
            Self::WhoIs(ip) => {
                if ip.is_unspecified()
                    || ip.is_multicast()
                    || ip.is_loopback()
                    || matches!(ip, IpAddr::V6(v) if v.to_ipv4_mapped().is_some())
                    || matches!(ip, IpAddr::V4(v) if v.is_broadcast())
                {
                    return Err(Error::InvalidEndpoint);
                }
                match ip {
                    IpAddr::V4(ip) => {
                        bytes[..4].copy_from_slice(&ip.octets());
                        Ok((4, bytes))
                    }
                    IpAddr::V6(ip) => Ok((6, ip.octets())),
                }
            }
        }
    }
}

/// Content-free native refusals; child output and OS diagnostics are not errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Unavailable,
    Untrusted,
    Busy,
    InvalidEndpoint,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// The verified child's code-directory identity, not its short-lived PID.
/// No public constructor can turn a caller's digest into authenticated evidence.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SourceIdentity {
    digest: [u8; 64],
    length: u8,
}
impl fmt::Debug for SourceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SourceIdentity([verified installed image])")
    }
}

/// A single bounded nonblocking read. EOF alone is not successful completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    Pending,
    Eof,
    Bytes(usize),
}

// Retain the original audited native source paths: the small platform crate
// owns their compilation, while fr-native can depend on the safe fr-tailnet
// facade without creating fr-tailnet -> fr-native -> frd -> fr-tailnet cycles.
#[cfg(target_os = "macos")]
#[path = "../../fr-native/src/tailnet_macos/native.rs"]
mod native;
#[cfg(target_os = "macos")]
pub use native::Snapshot;

// Keeps the macOS-only encoder compiled (and its tests run) on other targets.
#[cfg(not(target_os = "macos"))]
#[allow(clippy::type_complexity)]
const _: fn(Command) -> Result<(u8, [u8; 16]), Error> = Command::wire;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_commands_carry_only_numeric_address_bytes() {
        assert_eq!(Command::Status.wire(), Ok((0, [0; 16])));
        let (kind, bytes) = Command::WhoIs("100.64.0.2".parse().unwrap())
            .wire()
            .unwrap();
        assert_eq!(kind, 4);
        assert_eq!(&bytes[..4], &[100, 64, 0, 2]);
        assert_eq!(&bytes[4..], &[0; 12]);
        let ip: IpAddr = "fd7a:115c:a1e0::2".parse().unwrap();
        let IpAddr::V6(v6) = ip else { unreachable!() };
        assert_eq!(Command::WhoIs(ip).wire(), Ok((6, v6.octets())));
    }

    #[test]
    fn invalid_address_classes_refuse_before_native_spawn() {
        for address in [
            "0.0.0.0",
            "127.0.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "ff02::1",
            "::ffff:100.64.0.2",
        ] {
            assert_eq!(
                Command::WhoIs(address.parse().unwrap()).wire(),
                Err(Error::InvalidEndpoint),
                "{address}"
            );
        }
    }

    #[test]
    fn command_and_source_debug_do_not_disclose_identity() {
        let address = "100.64.0.2";
        assert!(!format!("{:?}", Command::WhoIs(address.parse().unwrap())).contains(address));
        assert_eq!(
            format!(
                "{:?}",
                SourceIdentity {
                    digest: [123; 64],
                    length: 32
                }
            ),
            "SourceIdentity([verified installed image])"
        );
    }
}
