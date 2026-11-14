//! Bounded local inputs for the two already-pinned public trust anchors.
//! A filename, PEM label or certificate name is never trust evidence: both
//! DER digests must match before the build can publish either output file.
#![forbid(unsafe_code)]

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest as _, Sha256};
use std::{fmt, fs::File, io::Read as _, path::Path};

pub const LOCAL_ROOTS_ENV: &str = "FR_MOBILE_ISRG_ROOTS_DIR";
pub const MAX_PEM_BYTES: usize = 8 * 1024;
const MAX_DER_BYTES: usize = 4 * 1024;
const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
const END: &str = "-----END CERTIFICATE-----";

#[derive(Clone, Copy)]
pub struct Root {
    pub name: &'static str,
    pub fingerprint: &'static str,
    pub file: &'static str,
}

/// SHA-256 of the exact DER roots already embedded by the Apple build path.
pub const ISRG_ROOTS: [Root; 2] = [
    Root {
        name: "ISRG Root X1",
        fingerprint: "96BCEC06264976F37460779ACF28C5A7CFE8A3C0AAE11A8FFCEE05C0BDDF08C6",
        file: "isrg-root-x1.pem",
    },
    Root {
        name: "ISRG Root X2",
        fingerprint: "69729B8E15A86EFC177A57AFB7171DFC64ADD28C2FCA8CF1507E34453CCB1470",
        file: "isrg-root-x2.pem",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Directory,
    File,
    Bound,
    Encoding,
    Envelope,
    Base64,
    Fingerprint,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Directory => "root directory must be an existing absolute directory",
            Self::File => "root must be a readable regular file",
            Self::Bound => "root exceeds the PEM or DER byte bound",
            Self::Encoding => "root PEM must be ASCII text",
            Self::Envelope => "root must contain exactly one CERTIFICATE PEM block",
            Self::Base64 => "root PEM contains malformed or noncanonical base64",
            Self::Fingerprint => "root DER SHA-256 does not match its pinned ISRG fingerprint",
        })
    }
}

/// Read both named regular files, validate their exact pins, then return
/// canonical PEM. Bounded reads also cover a file growing after metadata.
pub fn local_roots(directory: &Path) -> Result<[String; 2], Error> {
    if !directory.is_absolute() || !directory.is_dir() {
        return Err(Error::Directory);
    }
    Ok([
        read_root(directory, ISRG_ROOTS[0])?,
        read_root(directory, ISRG_ROOTS[1])?,
    ])
}

fn read_root(directory: &Path, root: Root) -> Result<String, Error> {
    let path = directory.join(root.file);
    // Reject a named pipe/device before opening it. The descriptor is checked
    // again below, and the read remains bounded if a regular file changes.
    if !path.metadata().map_err(|_| Error::File)?.is_file() {
        return Err(Error::File);
    }
    let file = File::open(path).map_err(|_| Error::File)?;
    let metadata = file.metadata().map_err(|_| Error::File)?;
    if !metadata.is_file() {
        return Err(Error::File);
    }
    if metadata.len() > MAX_PEM_BYTES as u64 {
        return Err(Error::Bound);
    }
    let mut pem = Vec::new();
    file.take(MAX_PEM_BYTES as u64 + 1)
        .read_to_end(&mut pem)
        .map_err(|_| Error::File)?;
    validate_pem(&pem, root)
}

/// Validate bytes, never names or an external command's digest annotation.
pub fn validate_pem(pem: &[u8], root: Root) -> Result<String, Error> {
    let der = decode_one(pem)?;
    let digest = Sha256::digest(&der);
    let hex = b"0123456789ABCDEF";
    let pinned = root.fingerprint.as_bytes();
    if pinned.len() != 64
        || digest.iter().enumerate().any(|(index, byte)| {
            pinned[index * 2] != hex[usize::from(byte >> 4)]
                || pinned[index * 2 + 1] != hex[usize::from(byte & 15)]
        })
    {
        return Err(Error::Fingerprint);
    }
    Ok(encode_pem(&der))
}

fn decode_one(pem: &[u8]) -> Result<Vec<u8>, Error> {
    if pem.len() > MAX_PEM_BYTES {
        return Err(Error::Bound);
    }
    if !pem.is_ascii() {
        return Err(Error::Encoding);
    }
    let text = std::str::from_utf8(pem).map_err(|_| Error::Encoding)?;
    let text = text.trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'));
    let mut lines = text.lines();
    if lines.next() != Some(BEGIN) {
        return Err(Error::Envelope);
    }
    let mut encoded = String::new();
    let mut ended = false;
    for line in lines.by_ref() {
        if line == END {
            ended = true;
            break;
        }
        if line.is_empty()
            || !line
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
        {
            return Err(Error::Base64);
        }
        encoded.push_str(line);
        if encoded.len() > MAX_DER_BYTES.div_ceil(3) * 4 {
            return Err(Error::Bound);
        }
    }
    if !ended || encoded.is_empty() || lines.next().is_some() {
        return Err(Error::Envelope);
    }
    let der = STANDARD.decode(encoded).map_err(|_| Error::Base64)?;
    if der.len() > MAX_DER_BYTES {
        return Err(Error::Bound);
    }
    Ok(der)
}

fn encode_pem(der: &[u8]) -> String {
    let encoded = STANDARD.encode(der);
    let mut pem = String::from(BEGIN);
    pem.push('\n');
    for line in encoded.as_bytes().chunks(64) {
        // Base64's alphabet is ASCII; no certificate bytes enter diagnostics.
        for byte in line {
            pem.push(char::from(*byte));
        }
        pem.push('\n');
    }
    pem.push_str(END);
    pem.push('\n');
    pem
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_certificate_accepts_lf_crlf_and_outer_whitespace() {
        let original = [0x30, 0x02, 0x00, 0xff];
        let canonical = encode_pem(&original);
        for value in [
            canonical.clone(),
            canonical.replace('\n', "\r\n"),
            format!(" \t\r\n{canonical}\r\n\t "),
        ] {
            assert_eq!(decode_one(value.as_bytes()).unwrap(), original);
        }
    }

    #[test]
    fn a_valid_pem_with_a_wrong_certificate_never_matches_either_pin() {
        let pem = encode_pem(b"not an ISRG certificate");
        for root in ISRG_ROOTS {
            assert!(root.name.starts_with("ISRG Root X"));
            assert_eq!(validate_pem(pem.as_bytes(), root), Err(Error::Fingerprint));
        }
    }

    #[test]
    fn multiple_blocks_trailing_content_and_incomplete_envelopes_are_refused() {
        let pem = encode_pem(b"a");
        for value in [
            format!("{pem}{pem}"),
            format!("{pem}comment"),
            format!("comment\n{pem}"),
            pem.replace(END, ""),
            pem.replace(BEGIN, "-----BEGIN PRIVATE KEY-----"),
            format!("{BEGIN}\n{END}\n"),
            String::new(),
        ] {
            assert!(decode_one(value.as_bytes()).is_err());
        }
    }

    #[test]
    fn malformed_base64_and_noncanonical_padding_are_refused() {
        for encoded in ["YQ=", "YQ===", "YQ", "YR==", "Y Q==", "YQ==!", "YQ==\0", ""] {
            let pem = format!("{BEGIN}\n{encoded}\n{END}\n");
            assert!(decode_one(pem.as_bytes()).is_err(), "{encoded:?}");
        }
        let pem = format!("{BEGIN}\nYQ==\n\n{END}\n");
        assert!(decode_one(pem.as_bytes()).is_err());
    }

    #[test]
    fn pem_and_decoded_size_caps_are_enforced_before_unbounded_allocation() {
        assert_eq!(
            decode_one(&vec![b'a'; MAX_PEM_BYTES + 1]),
            Err(Error::Bound)
        );
        let exact = vec![0x30; MAX_DER_BYTES];
        assert_eq!(decode_one(encode_pem(&exact).as_bytes()).unwrap(), exact);
        assert_eq!(
            decode_one(encode_pem(&vec![0x30; MAX_DER_BYTES + 1]).as_bytes()),
            Err(Error::Bound)
        );
        assert_eq!(decode_one(&[0xff]), Err(Error::Encoding));
    }

    #[test]
    fn relative_or_missing_directories_never_select_ambient_roots() {
        assert_eq!(local_roots(Path::new(".")), Err(Error::Directory));
        assert_eq!(local_roots(Path::new("")), Err(Error::Directory));
    }

    /// No certificate fixture is checked in. Point the explicit build input
    /// at the two public local roots and run this test with `--ignored`.
    #[test]
    #[ignore = "requires FR_MOBILE_ISRG_ROOTS_DIR with the two public pinned root files"]
    fn actual_local_isrg_roots_match_both_pins_and_refuse_swaps_or_mutation() {
        let directory = std::env::var_os(LOCAL_ROOTS_ENV).expect("explicit local root directory");
        let roots = local_roots(Path::new(&directory)).unwrap();
        for (index, root) in ISRG_ROOTS.into_iter().enumerate() {
            assert_eq!(
                validate_pem(roots[index].as_bytes(), root).unwrap(),
                roots[index]
            );
            assert_eq!(
                validate_pem(roots[1 - index].as_bytes(), root),
                Err(Error::Fingerprint)
            );
            let mut der = decode_one(roots[index].as_bytes()).unwrap();
            *der.last_mut().unwrap() ^= 1;
            assert_eq!(
                validate_pem(encode_pem(&der).as_bytes(), root),
                Err(Error::Fingerprint)
            );
        }
    }
}
