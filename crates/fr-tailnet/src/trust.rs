//! Locally configured platform trust roots for Tailscale-issued certificates.
//! Roots always come from a local file chosen by the operator (by default the
//! distribution CA bundle), never from the contacted peer or `LocalAPI` output.
use asupersync::tls::{Certificate, RootCertStore};
use std::{
    fmt,
    fs::File,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// Debian/Ubuntu/Arch/Fedora-compatible distribution bundle location. It is a
/// regular file on Debian/Ubuntu and a symlink into the extracted trust store
/// on Fedora/RHEL and Arch; see [`resolve_protected`].
pub const SYSTEM_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
const MAX_BYTES: u64 = 1 << 20;
const MAX_CERTIFICATES: usize = 512;
const MAX_LINK_HOPS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustError {
    /// Missing, unreadable, a symlink, or not a regular file.
    Unreadable,
    /// Larger than 1 MiB or more than 512 certificates.
    TooLarge,
    /// Not PEM, or no certificate the TLS stack accepts as a root.
    Invalid,
}
impl fmt::Display for TrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "trust-roots: {self:?}")
    }
}
impl std::error::Error for TrustError {}

/// Follow a root-owned symlink chain to a root-owned regular file, at most eight
/// hops. Every link must be owned by root, and every directory holding a link or
/// the file (lexically and after resolution) must be root-owned and not
/// group/other-writable, so no unprivileged user can redirect the trust anchor.
pub fn resolve_protected(path: &Path) -> Result<PathBuf, TrustError> {
    let mut current = path.to_path_buf();
    for _ in 0..=MAX_LINK_HOPS {
        let meta = std::fs::symlink_metadata(&current).map_err(|_| TrustError::Unreadable)?;
        let parent = current.parent().ok_or(TrustError::Unreadable)?;
        protected_directories(parent)?;
        if meta.uid() != 0 {
            return Err(TrustError::Unreadable);
        }
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&current).map_err(|_| TrustError::Unreadable)?;
            current = parent.join(target);
            continue;
        }
        if meta.is_file() && meta.mode() & 0o022 == 0 {
            return Ok(current);
        }
        return Err(TrustError::Unreadable);
    }
    Err(TrustError::Unreadable)
}

/// Every lexical ancestor (a root-owned directory or root-owned link) and every
/// resolved ancestor directory is root-owned and not group/other-writable.
fn protected_directories(dir: &Path) -> Result<(), TrustError> {
    let resolved = std::fs::canonicalize(dir).map_err(|_| TrustError::Unreadable)?;
    for ancestor in dir.ancestors().filter(|a| !a.as_os_str().is_empty()) {
        let meta = std::fs::symlink_metadata(ancestor).map_err(|_| TrustError::Unreadable)?;
        let writable = !meta.file_type().is_symlink() && meta.mode() & 0o022 != 0;
        if meta.uid() != 0 || writable || !(meta.is_dir() || meta.file_type().is_symlink()) {
            return Err(TrustError::Unreadable);
        }
    }
    for ancestor in resolved.ancestors() {
        let meta = std::fs::metadata(ancestor).map_err(|_| TrustError::Unreadable)?;
        if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return Err(TrustError::Unreadable);
        }
    }
    Ok(())
}

/// Parse a bounded PEM bundle. An operator-supplied path must be the regular
/// file itself (a symlink is refused). Only the distribution default
/// [`SYSTEM_BUNDLE`] may be a link, followed through [`resolve_protected`].
pub fn read_certificates(path: &Path) -> Result<Vec<Certificate>, TrustError> {
    let resolved;
    let path = if path == Path::new(SYSTEM_BUNDLE) {
        resolved = resolve_protected(path)?;
        resolved.as_path()
    } else {
        path
    };
    if !std::fs::symlink_metadata(path)
        .map_err(|_| TrustError::Unreadable)?
        .is_file()
    {
        return Err(TrustError::Unreadable);
    }
    let file = File::open(path).map_err(|_| TrustError::Unreadable)?;
    if !file
        .metadata()
        .map_err(|_| TrustError::Unreadable)?
        .is_file()
    {
        return Err(TrustError::Unreadable);
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| TrustError::Unreadable)?;
    let marker = b"-----BEGIN CERTIFICATE-----";
    if u64::try_from(bytes.len()).map_or(true, |n| n > MAX_BYTES)
        || bytes.windows(marker.len()).filter(|s| *s == marker).count() > MAX_CERTIFICATES
    {
        return Err(TrustError::TooLarge);
    }
    let certificates = Certificate::from_pem(&bytes).map_err(|_| TrustError::Invalid)?;
    if certificates.is_empty() {
        return Err(TrustError::Invalid);
    }
    Ok(certificates)
}

/// Build a root store from a bundle. Distribution bundles can carry entries the
/// TLS stack rejects as trust anchors; those are skipped (reducing trust, never
/// widening it). At least one accepted root is required.
pub fn root_store(path: &Path) -> Result<RootCertStore, TrustError> {
    let mut roots = RootCertStore::empty();
    for certificate in read_certificates(path)? {
        let _ = roots.add(&certificate);
    }
    if roots.is_empty() {
        return Err(TrustError::Invalid);
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fr-trust-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bundle.pem");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        path
    }

    #[test]
    fn system_bundle_loads_more_than_sixty_four_roots_when_present() {
        let path = Path::new(SYSTEM_BUNDLE);
        if !path.exists() {
            eprintln!("SKIPPED: no distribution CA bundle at {SYSTEM_BUNDLE}");
            return;
        }
        // The default path itself, regular file or protected link alike.
        let roots = root_store(path).unwrap();
        assert!(roots.len() > 64, "{}", roots.len());
    }

    #[test]
    fn protected_resolution_follows_root_owned_links_and_refuses_user_links() {
        // A distribution-owned certificate link (Debian/Ubuntu layout).
        let system = Path::new("/etc/ssl/certs/ISRG_Root_X1.pem");
        if system.exists() {
            let resolved = resolve_protected(system).unwrap();
            assert!(!std::fs::symlink_metadata(&resolved).unwrap().is_symlink());
            assert_eq!(read_certificates(&resolved).unwrap().len(), 1);
        } else {
            eprintln!("SKIPPED positive half: no {}", system.display());
        }
        // A link an unprivileged user could plant is never followed, whatever
        // it points at, and a user-owned regular file is not a trust anchor.
        let target = temp("user-link-target", b"x");
        let link = target.with_file_name("user-link.pem");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(system, &link).unwrap();
        assert_eq!(resolve_protected(&link).err(), Some(TrustError::Unreadable));
        assert_eq!(
            resolve_protected(&target).err(),
            Some(TrustError::Unreadable)
        );
        assert_eq!(
            resolve_protected(Path::new("/nonexistent/bundle")).err(),
            Some(TrustError::Unreadable)
        );
    }

    #[test]
    fn refuses_non_pem_empty_symlink_and_oversize_input() {
        assert_eq!(
            read_certificates(&temp("garbage", b"not a certificate")).err(),
            Some(TrustError::Invalid)
        );
        assert_eq!(
            read_certificates(Path::new("/nonexistent/fr-trust")).err(),
            Some(TrustError::Unreadable)
        );
        let target = temp("link-target", b"x");
        let link = target.with_file_name("link.pem");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(read_certificates(&link).err(), Some(TrustError::Unreadable));
        let big = vec![b'a'; usize::try_from(MAX_BYTES).unwrap() + 1];
        assert_eq!(
            read_certificates(&temp("big", &big)).err(),
            Some(TrustError::TooLarge)
        );
        let many = "-----BEGIN CERTIFICATE-----\n".repeat(MAX_CERTIFICATES + 1);
        assert_eq!(
            read_certificates(&temp("many", many.as_bytes())).err(),
            Some(TrustError::TooLarge)
        );
    }
}
