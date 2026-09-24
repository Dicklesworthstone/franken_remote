//! Locally configured platform trust roots for Tailscale-issued certificates.
//! Roots always come from a local file chosen by the operator (by default the
//! distribution CA bundle), never from the contacted peer or `LocalAPI` output.
use asupersync::tls::{Certificate, RootCertStore};
use std::{fmt, fs::File, io::Read, path::Path};

/// Debian/Ubuntu/Arch/Fedora-compatible distribution bundle location.
pub const SYSTEM_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
const MAX_BYTES: u64 = 1 << 20;
const MAX_CERTIFICATES: usize = 512;

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

/// Parse a bounded PEM bundle. The path itself must be a regular file (a
/// symlink is refused, so point at the resolved bundle).
pub fn read_certificates(path: &Path) -> Result<Vec<Certificate>, TrustError> {
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
        // The bundle is usually a symlink target list; resolve like an operator would.
        let resolved = std::fs::canonicalize(path).unwrap();
        let roots = root_store(&resolved).unwrap();
        assert!(roots.len() > 64, "{}", roots.len());
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
