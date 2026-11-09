//! The macOS standalone Tailscale client (its system network extension)
//! serves `LocalAPI` as HTTP on a loopback TCP port. On every start it
//! publishes that port as the root-owned symlink `ipnport` and a per-run
//! secret in `sameuserproof-PORT` (root:admin, 0640), both in the root-owned
//! `/Library/Tailscale`. A loopback socket carries no peer credentials, so
//! trust comes from those files: an unprivileged process can neither write
//! them nor replace them in a directory it cannot write. Only an admin user
//! can read the secret, which Tailscale requires as HTTP Basic credentials.
//!
//! The daemon instance stands in for Linux's peer PID. It is derived from the
//! port, secret and publication time, which change when the daemon restarts,
//! so a restart between queries still breaks the identity snapshot.
//!
//! Tailscale's own CLI reaches the daemon exactly this way: while it runs it
//! holds `127.0.0.1:PORT` connections and reads the same token. A client
//! cannot learn which process accepted its loopback connection (the kernel's
//! socket list shows an unprivileged caller only its own sockets), so neither
//! this reader nor the CLI authenticates the listener beyond these files.
//!
//! No subprocess, no ambient HTTP proxy, no other directory. The App Store
//! variant (sandboxed, group container) is not supported here.
use crate::Error;
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// The installed state directory of the standalone macOS client.
pub(super) const INSTALLED: &str = "/Library/Tailscale";
const MAX_SECRET_BYTES: u64 = 256;

pub(super) struct Endpoint {
    pub(super) port: u16,
    secret: String,
    pub(super) instance: i32,
}
impl Endpoint {
    /// `Authorization` value: Basic credentials with an empty user name.
    pub(super) fn authorization(&self) -> String {
        let mut credentials = String::with_capacity(self.secret.len() + 1);
        credentials.push(':');
        credentials.push_str(&self.secret);
        format!("Basic {}", base64(credentials.as_bytes()))
    }
}

/// Root-owned, and not writable by group or others.
fn protected(metadata: &fs::Metadata) -> bool {
    metadata.uid() == 0 && metadata.mode() & 0o022 == 0
}

/// Reads the daemon's published endpoint, refusing anything not published
/// by root. Missing files mean the daemon is not running (or not this
/// variant); an unreadable secret means this user is not an administrator.
pub(super) fn endpoint(directory: &Path) -> Result<Endpoint, Error> {
    let unavailable = |_| Error::LocalApiUnavailable;
    let dir = fs::symlink_metadata(directory).map_err(unavailable)?;
    if !dir.is_dir() || !protected(&dir) {
        return Err(Error::UntrustedLocalApi);
    }
    let link = directory.join("ipnport");
    let meta = fs::symlink_metadata(&link).map_err(unavailable)?;
    if !meta.file_type().is_symlink() || meta.uid() != 0 {
        return Err(Error::UntrustedLocalApi);
    }
    let port = port(&fs::read_link(&link).map_err(unavailable)?)?;
    let proof: PathBuf = directory.join(format!("sameuserproof-{port}"));
    let meta = fs::symlink_metadata(&proof).map_err(unavailable)?;
    if !meta.is_file() || meta.uid() != 0 || meta.mode() & 0o002 != 0 {
        return Err(Error::UntrustedLocalApi);
    }
    if meta.len() > MAX_SECRET_BYTES {
        return Err(Error::UntrustedLocalApi);
    }
    let secret = fs::read_to_string(&proof).map_err(|_| Error::LocalApiDenied)?;
    let secret = secret.trim();
    if secret.is_empty() || !secret.bytes().all(|b| b.is_ascii_graphic() && b != b':') {
        return Err(Error::UntrustedLocalApi);
    }
    let instance = instance(port, secret, meta.mtime(), meta.mtime_nsec());
    Ok(Endpoint {
        port,
        secret: secret.to_owned(),
        instance,
    })
}

/// A decimal port number, exactly as the daemon writes the link target.
fn port(target: &Path) -> Result<u16, Error> {
    target
        .to_str()
        .filter(|s| (1..=5).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|&p| p != 0)
        .ok_or(Error::UntrustedLocalApi)
}

/// A positive identifier for one daemon run (FNV-1a over its publication).
fn instance(port: u16, secret: &str, seconds: i64, nanos: i64) -> i32 {
    let mut hash: u32 = 0x811c_9dc5;
    let bytes = port
        .to_be_bytes()
        .into_iter()
        .chain(secret.bytes())
        .chain(seconds.to_be_bytes())
        .chain(nanos.to_be_bytes());
    for byte in bytes {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    // Clear the sign bit and avoid zero: a valid "PID" is positive.
    i32::try_from(hash & 0x7fff_ffff).map_or(1, |v| v.max(1))
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected);
        }
    }

    #[test]
    fn ports_are_plain_nonzero_decimals() {
        assert_eq!(port(Path::new("49280")), Ok(49280));
        for bad in ["0", "", "65536", "+80", "80 ", "../80", "123456", "0x50"] {
            assert_eq!(
                port(Path::new(bad)),
                Err(Error::UntrustedLocalApi),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn instances_are_positive_and_change_with_any_input() {
        let a = instance(49280, "secret", 1, 2);
        assert!(a > 0);
        assert_ne!(a, instance(49281, "secret", 1, 2));
        assert_ne!(a, instance(49280, "secreu", 1, 2));
        assert_ne!(a, instance(49280, "secret", 2, 2));
        assert_ne!(a, instance(49280, "secret", 1, 3));
        assert_eq!(a, instance(49280, "secret", 1, 2));
    }

    #[test]
    fn a_directory_this_user_owns_is_refused() {
        // The scratch directory is not root-owned: refused before any read.
        let dir = std::env::temp_dir();
        assert_eq!(endpoint(&dir).err(), Some(Error::UntrustedLocalApi));
        assert_eq!(
            endpoint(Path::new("/nonexistent/fr-tailnet")).err(),
            Some(Error::LocalApiUnavailable)
        );
    }

    #[test]
    fn authorization_is_basic_with_an_empty_user() {
        let e = Endpoint {
            port: 1,
            secret: "abc".into(),
            instance: 1,
        };
        assert_eq!(e.authorization(), "Basic OmFiYw==");
    }
}
