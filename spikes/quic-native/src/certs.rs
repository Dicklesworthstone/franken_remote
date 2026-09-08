//! Runtime-generated test PKI: one CA, one leaf for localhost/127.0.0.1, and a
//! second unrelated CA for the negative-trust scenario. Real WebPKI path only —
//! no skip-verify anywhere in this spike.

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub struct TestPki {
    pub ca_der: CertificateDer<'static>,
    pub leaf_der: CertificateDer<'static>,
    pub leaf_key: PrivateKeyDer<'static>,
    /// A second, unrelated CA that did NOT sign the leaf.
    pub wrong_ca_der: CertificateDer<'static>,
}

fn new_ca(common_name: &str) -> (rcgen::Certificate, KeyPair) {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let key = KeyPair::generate().expect("generate CA key");
    let cert = params.self_signed(&key).expect("self-sign CA");
    (cert, key)
}

impl TestPki {
    pub fn generate() -> Self {
        let (ca_cert, ca_key) = new_ca("frankenremote-spike-ca");
        let (wrong_ca_cert, _wrong_ca_key) = new_ca("frankenremote-spike-wrong-ca");

        let mut leaf_params =
            CertificateParams::new(vec!["localhost".to_string()]).expect("leaf params");
        leaf_params
            .distinguished_name
            .push(DnType::CommonName, "frankenremote-spike-leaf");
        leaf_params.subject_alt_names = vec![
            SanType::DnsName("localhost".try_into().expect("dns san")),
            SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        ];
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let leaf_key = KeyPair::generate().expect("generate leaf key");
        let leaf_cert = leaf_params
            .signed_by(&leaf_key, &ca_cert, &ca_key)
            .expect("sign leaf");

        Self {
            ca_der: ca_cert.der().clone().into_owned(),
            leaf_der: leaf_cert.der().clone().into_owned(),
            leaf_key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
            wrong_ca_der: wrong_ca_cert.der().clone().into_owned(),
        }
    }
}
