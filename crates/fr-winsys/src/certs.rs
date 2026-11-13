//! The machine's trusted root certificates: the Local Machine `ROOT` system
//! store (with the third-party roots Windows has installed under it), which
//! only an administrator can change. Read-only; nothing here adds, removes or
//! fetches a certificate.
use std::io;
use windows::Win32::Security::Cryptography::{
    CERT_OPEN_STORE_FLAGS, CERT_QUERY_ENCODING_TYPE, CERT_STORE_OPEN_EXISTING_FLAG,
    CERT_STORE_PROV_SYSTEM_W, CERT_STORE_READONLY_FLAG, CERT_SYSTEM_STORE_LOCAL_MACHINE,
    CertCloseStore, CertEnumCertificatesInStore, CertOpenStore, HCERTSTORE, X509_ASN_ENCODING,
};
use windows::core::w;

/// Closes the store on every path out.
struct Store(HCERTSTORE);
impl Drop for Store {
    fn drop(&mut self) {
        // SAFETY: the handle came from CertOpenStore and is closed once.
        let _ = unsafe { CertCloseStore(Some(self.0), 0) };
    }
}

/// The DER encoding of every X.509 certificate in the Local Machine `ROOT`
/// store. More than `max_count` certificates or `max_bytes` of encodings in
/// all is `InvalidData`; a store that cannot be opened is the open's error.
pub fn machine_roots(max_count: usize, max_bytes: usize) -> io::Result<Vec<Vec<u8>>> {
    let flags = CERT_OPEN_STORE_FLAGS(
        CERT_SYSTEM_STORE_LOCAL_MACHINE
            | CERT_STORE_READONLY_FLAG.0
            | CERT_STORE_OPEN_EXISTING_FLAG.0,
    );
    // SAFETY: the system-store provider takes a NUL-terminated UTF-16 store
    // name as its parameter; `w!` yields a static one.
    let store = Store(unsafe {
        CertOpenStore(
            CERT_STORE_PROV_SYSTEM_W,
            CERT_QUERY_ENCODING_TYPE(0),
            None,
            flags,
            Some(w!("ROOT").as_ptr().cast()),
        )
    }?);
    let mut certificates = Vec::new();
    let (mut count, mut bytes) = (0usize, 0usize);
    let mut previous = None;
    loop {
        // SAFETY: `previous` is the context this loop received last (or
        // None); the call frees it and returns the next, or null at the end
        // (having freed the last one). Running to the end frees every one.
        let context = unsafe { CertEnumCertificatesInStore(store.0, previous) };
        if context.is_null() {
            break;
        }
        previous = Some(context.cast_const());
        // SAFETY: a non-null context from the enumeration stays valid until
        // the next call; its encoding is `cbCertEncoded` bytes.
        let (encoding, der) = unsafe {
            let context = &*context;
            let length = usize::try_from(context.cbCertEncoded).unwrap_or(usize::MAX);
            let der = if context.pbCertEncoded.is_null() || length > max_bytes {
                None
            } else {
                Some(std::slice::from_raw_parts(context.pbCertEncoded, length))
            };
            (context.dwCertEncodingType, der)
        };
        if encoding.0 & X509_ASN_ENCODING.0 == 0 {
            continue;
        }
        count += 1;
        bytes = bytes.saturating_add(der.map_or(usize::MAX, <[u8]>::len));
        if count <= max_count && bytes <= max_bytes {
            certificates.push(der.unwrap_or_default().to_vec());
        }
    }
    if count > max_count || bytes > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the ROOT store holds more than the bound allows",
        ));
    }
    Ok(certificates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_machine_root_store_reads_within_its_bounds() {
        let roots = machine_roots(4096, 16 << 20).unwrap();
        eprintln!(
            "{} roots, {} bytes",
            roots.len(),
            roots.iter().map(Vec::len).sum::<usize>()
        );
        assert_ne!(roots.len(), 0);
        // Every entry is a DER SEQUENCE.
        assert!(roots.iter().all(|der| der.first() == Some(&0x30)));
        // A bound below the store's size refuses rather than truncating.
        let refused = machine_roots(roots.len() - 1, 16 << 20).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidData);
        let total = roots.iter().map(Vec::len).sum::<usize>();
        let refused = machine_roots(4096, total - 1).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidData);
    }
}
