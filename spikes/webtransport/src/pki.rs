use rcgen::{
    CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose, SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime};

pub struct WebTransportPki {
    pub cert_der: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
    pub cert_hash: [u8; 32],
    pub cert_hash_hex: String,
}

impl WebTransportPki {
    pub fn generate() -> Result<Self, Box<dyn std::error::Error>> {
        let mut params = CertificateParams::new(vec!["localhost".to_string()])?;
        params
            .distinguished_name
            .push(DnType::CommonName, "frankenremote-webtransport-leaf");
        params.subject_alt_names = vec![
            SanType::DnsName("localhost".try_into()?),
            SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        ];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

        // WebTransport in browsers with serverCertificateHashes requires:
        // 1. ECDSA P-256 key
        // 2. Validity period <= 14 days
        let now = SystemTime::now();
        let not_before = now.checked_sub(Duration::from_secs(3600)).unwrap_or(now);
        let not_after = now + Duration::from_secs(10 * 86400); // 10 days validity (< 14 days)

        params.not_before = not_before.into();
        params.not_after = not_after.into();

        // Generate ECDSA P-256 key pair
        let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let cert = params.self_signed(&key_pair)?;

        let der_bytes = cert.der().to_vec();
        let mut hasher = Sha256::new();
        hasher.update(&der_bytes);
        let hash_result = hasher.finalize();
        let mut cert_hash = [0u8; 32];
        cert_hash.copy_from_slice(&hash_result);
        let mut s = String::with_capacity(64);
        for b in cert_hash {
            use std::fmt::Write;
            let _ = write!(s, "{:02x}", b);
        }
        let cert_hash_hex = s;

        let cert_der = CertificateDer::from(der_bytes);
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

        Ok(Self {
            cert_der,
            key: key_der,
            cert_hash,
            cert_hash_hex,
        })
    }
}
