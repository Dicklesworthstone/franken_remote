//! The trust roots the iOS client verifies hosts against.
//!
//! Tailscale issues `*.ts.net` machine certificates through Let's Encrypt,
//! whose chains end at ISRG Root X1 (RSA) or ISRG Root X2 (ECDSA). The app
//! cannot hand iOS's own trust store to rustls, so it carries exactly these
//! two public roots. The build copies them from the build Mac's
//! `SystemRootCertificates` keychain, selected by pinned fingerprint
//! (`build.rs`; the repository keeps no certificate files), and the tests
//! below check the embedded bytes again. A host whose certificate chains to
//! any other root is refused. If Let's Encrypt moves Tailscale's chains to a
//! new root, this list (and an app update) must follow; nothing is learned
//! from the contacted peer.
use asupersync::tls::Certificate;

const ISRG_ROOT_X1: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/isrg-root-x1.pem"));
const ISRG_ROOT_X2: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/isrg-root-x2.pem"));

/// SHA-256 of each root's DER encoding, as published by Let's Encrypt.
pub const FINGERPRINTS: [&str; 2] = [
    "96bcec06264976f37460779acf28c5a7cfe8a3c0aae11a8ffcee05c0bddf08c6",
    "69729b8e15a86efc177a57afb7171dfc64add28c2fca8cf1507e34453ccb1470",
];

/// The bundled roots, parsed. Never empty (the tests prove both parse).
pub fn bundled() -> Result<Vec<Certificate>, fr_tailnet::Error> {
    let mut roots = Vec::with_capacity(2);
    for pem in [ISRG_ROOT_X1, ISRG_ROOT_X2] {
        let mut parsed =
            Certificate::from_pem(pem).map_err(|_| fr_tailnet::Error::InvalidTrustStore)?;
        if parsed.len() != 1 {
            return Err(fr_tailnet::Error::InvalidTrustStore);
        }
        roots.append(&mut parsed);
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn sha256_hex(bytes: &[u8]) -> String {
        use std::fmt::Write;
        let digest = system_sha256(bytes);
        digest.iter().fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
    }
    // SHA-256 from the system's `shasum`: an independent implementation, so
    // the check does not trust the TLS stack it guards. Unix only: the bundle
    // is the same compiled-in bytes on Windows, checked on the Apple hosts.
    #[cfg(unix)]
    fn system_sha256(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut child = std::process::Command::new("/usr/bin/shasum")
            .args(["-a", "256"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        let hex = text.split_whitespace().next().unwrap();
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn exactly_the_two_isrg_roots_are_bundled_with_their_published_fingerprints() {
        let roots = bundled().unwrap();
        assert_eq!(roots.len(), 2);
        let found: Vec<String> = roots.iter().map(|c| sha256_hex(c.as_der())).collect();
        assert_eq!(found, FINGERPRINTS);
        // Planted negative: one changed byte is a different certificate.
        let mut altered = roots[0].as_der().to_vec();
        let last = altered.len() - 1;
        altered[last] ^= 1;
        assert_ne!(sha256_hex(&altered), FINGERPRINTS[0]);
    }

    #[test]
    fn the_bundled_roots_build_the_strict_named_dialer() {
        let name = fr_tailnet::HostName::parse("mac.tailtest.ts.net").unwrap();
        fr_tailnet::NamedClient::new(name, bundled().unwrap(), std::time::Duration::from_secs(5))
            .unwrap();
    }
}
