//! SHA-256 hex digests for observation artifact hashing (FIPS 180-4). The
//! digest is fr-core's safe implementation, shared with clipboard images.

pub use fr_core::clipboard::image::sha256_digest;

/// Compute lowercase hex SHA-256 digest of input byte slice.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = sha256_digest(data);
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_empty() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_sha256_abc() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
