use super::*;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

fn destination(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "fr-observation-artifact-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir(&directory).unwrap();
    directory.join(name)
}

#[test]
fn checksum_vectors_and_png_header_are_canonical() {
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    assert_eq!(crc32(b"IEND"), 0xae42_6082);
    let png = encode(
        2,
        2,
        &[3, 2, 1, 255, 6, 5, 4, 128, 9, 8, 7, 64, 12, 11, 10, 0],
    )
    .unwrap();
    // Independent complete fixture generated with Python struct/zlib (level 0),
    // including RGBA conversion, both row filters, Adler-32 and all chunk CRCs.
    let golden = "89504e470d0a1a0a0000000d494844520000000200000002080600000072b60d240000001d494441547801011200edff00010203ff0405068000070809400a0b0c00165b020ee601840e0000000049454e44ae426082";
    let expected = (0..golden.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&golden[index..index + 2], 16).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(png, expected);
    assert_eq!(png[..8], SIGNATURE);
    assert_eq!(&png[12..16], b"IHDR");
    assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 2);
    assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 2);
    assert_eq!(&png[24..29], &[8, 6, 0, 0, 0]);
    assert_eq!(
        &png[png.len() - 12..],
        &[0, 0, 0, 0, b'I', b'E', b'N', b'D', 174, 66, 96, 130]
    );
}

#[test]
fn invalid_dimensions_and_pixel_bounds_never_create_a_file() {
    let path = destination("invalid.png");
    for (width, height, pixels) in [
        (0, 2, &[][..]),
        (2, 0, &[][..]),
        (8193, 2, &[][..]),
        (4096, 4096, &[][..]),
        (2, 2, &[0; 15][..]),
        (2, 2, &[0; 17][..]),
    ] {
        assert_eq!(
            write(&path, width, height, pixels).err().unwrap().code,
            "observation_artifact_invalid"
        );
        assert!(!path.exists());
    }
}

#[test]
fn exclusive_publication_reports_the_hash_and_preserves_existing_data() {
    let path = destination("actual.png");
    let pixels = [9, 8, 7, 255].repeat(4);
    let saved = write(&path, 2, 2, &pixels).unwrap();
    let original = std::fs::read(&path).unwrap();
    assert_eq!(saved.byte_count, original.len());
    assert_eq!(
        saved.sha256,
        fr_client::robot::sha256::sha256_hex(&original)
    );
    let failure = write(&path, 2, 2, &[0; 16]).err().unwrap();
    assert_eq!(failure.code, "observation_artifact_exists");
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!format!("{failure:?}").contains(path.to_str().unwrap()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o077,
            0
        );
    }
}

#[cfg(unix)]
#[test]
fn existing_and_dangling_symlinks_are_not_followed() {
    use std::os::unix::fs::symlink;
    let existing = destination("existing.png");
    std::fs::write(&existing, b"keep original").unwrap();
    let missing = existing.with_file_name("missing.png");
    for (name, target) in [("linked.png", &existing), ("dangling.png", &missing)] {
        let link = existing.with_file_name(name);
        symlink(target, &link).unwrap();
        assert_eq!(
            write(&link, 2, 2, &[0; 16]).err().unwrap().code,
            "observation_artifact_exists"
        );
    }
    assert_eq!(std::fs::read(existing).unwrap(), b"keep original");
    assert!(!missing.exists());
}

#[test]
fn multi_block_output_stays_within_the_checked_allocation() {
    let pixels = [3, 2, 1, 255].repeat(128 * 129);
    let png = encode(128, 129, &pixels).unwrap();
    assert_eq!(png.len(), 128 * 129 * 4 + 129 + 2 * 5 + 63);
    assert!(png.capacity() <= MAX_PNG_BYTES);
    assert_eq!(&png[37..41], b"IDAT");
    // Two stored blocks: the first cannot claim to be the final block.
    assert_eq!(png[43], 0);
    assert_eq!(png[43 + 5 + 65_535], 1);
}
