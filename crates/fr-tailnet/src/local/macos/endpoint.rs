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
//! so a restart between queries still breaks the identity snapshot. It is
//! never below 2^30, above every XNU PID, so it cannot equal the PID a Unix
//! socket read reports.
//!
//! The open-source daemon also keeps its state in `/Library/Tailscale`, but
//! root-only (0700) and without `ipnport`: that, like an absent directory or
//! link, is "nothing published" ([`published`] returns `None`), which selects
//! its Unix socket. Anything else that fails here is a refusal, never that.
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
    io::ErrorKind,
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
/// by root. `None` when nothing is published for this user: the directory or
/// its `ipnport` link is absent, or the protected directory denies this user
/// search (root-only). Other missing files mean the daemon is not running; an
/// unreadable secret means this user is not an administrator.
pub(super) fn published(directory: &Path) -> Result<Option<Endpoint>, Error> {
    let unavailable = |_| Error::LocalApiUnavailable;
    let dir = match fs::symlink_metadata(directory) {
        Ok(dir) => dir,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Error::LocalApiUnavailable),
    };
    // Checked before the link: a directory others can write could gain an
    // `ipnport` at any time, so its contents never select a transport.
    if !dir.is_dir() || !protected(&dir) {
        return Err(Error::UntrustedLocalApi);
    }
    let link = directory.join("ipnport");
    let meta = match fs::symlink_metadata(&link) {
        Ok(meta) => meta,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(None);
        }
        Err(_) => return Err(Error::LocalApiUnavailable),
    };
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
    Ok(Some(Endpoint {
        port,
        secret: secret.to_owned(),
        instance,
    }))
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

/// An identifier for one daemon run (FNV-1a over its publication), in
/// [2^30, 2^31): positive, and above XNU's `PID_MAX` (99,999).
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
    // Keep 30 hash bits and set bit 30: positive, and never a PID.
    i32::try_from((hash & 0x3fff_ffff) | 0x4000_0000).unwrap_or(i32::MAX)
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
        // Disjoint from Unix-socket PIDs (XNU PID_MAX is 99,999).
        for (port, secret) in [(1, "a"), (49280, "secret"), (65535, "~")] {
            assert!(instance(port, secret, -1, i64::MAX) >= 1 << 30);
        }
        assert_ne!(a, instance(49281, "secret", 1, 2));
        assert_ne!(a, instance(49280, "secreu", 1, 2));
        assert_ne!(a, instance(49280, "secret", 2, 2));
        assert_ne!(a, instance(49280, "secret", 1, 3));
        assert_eq!(a, instance(49280, "secret", 1, 2));
    }

    #[test]
    fn a_directory_this_user_owns_is_refused_not_treated_as_unpublished() {
        // The scratch directory is not root-owned: refused before any read,
        // even though it has no `ipnport` (no fallback to the socket).
        let dir = std::env::temp_dir();
        assert!(!dir.join("ipnport").exists());
        assert_eq!(published(&dir).err(), Some(Error::UntrustedLocalApi));
    }

    #[test]
    fn absent_or_root_only_state_directories_publish_nothing() {
        // No directory at all (App Store variant, or no Tailscale).
        assert!(matches!(
            published(Path::new("/nonexistent/fr-tailnet")),
            Ok(None)
        ));
        // Root-owned and searchable, but no `ipnport` link.
        assert!(matches!(published(Path::new("/usr/lib")), Ok(None)));
        // Root-owned 0750 root:wheel, which this (non-wheel) test user cannot
        // search, like the open-source daemon's 0700 `/Library/Tailscale`.
        let root_only = Path::new("/private/var/root");
        let meta = fs::symlink_metadata(root_only).unwrap();
        assert!(protected(&meta) && meta.mode() & 0o001 == 0);
        assert!(matches!(published(root_only), Ok(None)));
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
