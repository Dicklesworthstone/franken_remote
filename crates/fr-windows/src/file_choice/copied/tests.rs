use super::HEADER_BYTES;
use crate::file_choice::{
    Error, MAX_COPIED_BYTES, MAX_PATH_UNITS, MAX_SELECTED_FILES, copied_paths,
};

fn block(paths: &[&str]) -> Vec<u8> {
    let mut bytes = vec![0; HEADER_BYTES];
    bytes[..4].copy_from_slice(&u32::try_from(HEADER_BYTES).unwrap().to_le_bytes());
    bytes[16..20].copy_from_slice(&1_u32.to_le_bytes());
    for path in paths {
        for unit in path.encode_utf16().chain([0]) {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&[0, 0]);
    if paths.is_empty() {
        bytes.extend_from_slice(&[0, 0]);
    }
    bytes
}

fn decoded(bytes: &[u8]) -> Vec<String> {
    copied_paths(bytes)
        .unwrap()
        .into_iter()
        .map(|path| String::from_utf16(&path).unwrap())
        .collect()
}

#[test]
fn one_or_eight_complete_unicode_paths_keep_their_original_order() {
    let single = r"C:\chosen\Résumé final 🎉.txt";
    assert_eq!(decoded(&block(&[single])), [single]);
    let paths: Vec<_> = (0..MAX_SELECTED_FILES)
        .map(|i| format!("C:\\chosen\\file{i}.txt"))
        .collect();
    let names: Vec<_> = paths.iter().map(String::as_str).collect();
    assert_eq!(decoded(&block(&names)), paths);
}

#[test]
fn header_uses_checked_byte_offsets_and_requires_unicode() {
    let original = block(&[r"C:\chosen\one.txt"]);
    for offset in [0, 18, 19, 21, u32::MAX] {
        let mut bytes = original.clone();
        bytes[..4].copy_from_slice(&offset.to_le_bytes());
        assert_eq!(copied_paths(&bytes), Err(Error::InvalidPath));
    }
    for length in 0..HEADER_BYTES {
        assert_eq!(copied_paths(&original[..length]), Err(Error::InvalidPath));
    }
    let mut ansi = original.clone();
    ansi[16..20].fill(0);
    assert_eq!(copied_paths(&ansi), Err(Error::InvalidPath));
    let mut wide = original;
    wide[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(decoded(&wide), [r"C:\chosen\one.txt"]);
}

#[test]
fn native_offset_padding_and_bounded_allocator_padding_are_not_paths() {
    let original = block(&[r"C:\chosen\one.txt"]);
    let mut padded = original[..HEADER_BYTES].to_vec();
    padded[..4].copy_from_slice(&24_u32.to_le_bytes());
    padded.extend_from_slice(&[0x7f; 4]);
    padded.extend_from_slice(&original[HEADER_BYTES..]);
    // GlobalSize need not be the original requested byte count, even or zero
    // filled; only bytes through the double-NUL list are format content.
    padded.extend_from_slice(&[0x7f; 7]);
    assert_eq!(decoded(&padded), [r"C:\chosen\one.txt"]);
}

#[test]
fn malformed_or_empty_lists_never_publish_a_valid_prefix() {
    assert_eq!(copied_paths(&block(&[])), Err(Error::NoCopiedFiles));
    let original = block(&[r"C:\chosen\one.txt", r"C:\chosen\two.txt"]);
    for trim in [1, 2, 3, 4, 5] {
        assert_eq!(
            copied_paths(&original[..original.len() - trim]),
            Err(Error::InvalidPath)
        );
    }
    let mut invalid_unicode = block(&[r"C:\chosen\one.txt", r"C:\chosen\two.txt"]);
    let last_name_unit = invalid_unicode.len() - 6;
    invalid_unicode[last_name_unit..last_name_unit + 2].copy_from_slice(&0xd800_u16.to_le_bytes());
    assert_eq!(copied_paths(&invalid_unicode), Err(Error::InvalidPath));
}

#[test]
fn a_ninth_file_duplicate_or_unsafe_member_refuses_the_whole_selection() {
    let paths: Vec<_> = (0..=MAX_SELECTED_FILES)
        .map(|i| format!("C:\\chosen\\file{i}.txt"))
        .collect();
    let names: Vec<_> = paths.iter().map(String::as_str).collect();
    assert_eq!(copied_paths(&block(&names)), Err(Error::InvalidPath));
    let first = r"C:\chosen\one.txt";
    for invalid in [
        first,
        r"\\server\share\file.txt",
        r"\\?\C:\chosen\file.txt",
        r"C:relative.txt",
        r"C:\chosen\file.txt:secret",
        r"C:\chosen\..\file.txt",
        r"C:\chosen\bad.\file.txt",
        r"C:\chosen\CON.txt",
        r"C:\chosen\",
        "C:\\chosen\\line\nbreak.txt",
    ] {
        assert_eq!(
            copied_paths(&block(&[first, invalid])),
            Err(Error::InvalidPath)
        );
    }
}

#[test]
fn allocation_and_selected_path_bounds_are_both_enforced() {
    let path = format!("C:\\{}", "a".repeat(MAX_PATH_UNITS - 4));
    assert_eq!(copied_paths(&block(&[&path])).unwrap().len(), 1);
    let too_long = format!("{path}b");
    assert_eq!(copied_paths(&block(&[&too_long])), Err(Error::InvalidPath));
    let second = r"D:\b";
    assert_eq!(
        copied_paths(&block(&[&path, second])),
        Err(Error::InvalidPath)
    );
    let mut padded = block(&[r"C:\chosen\one.txt"]);
    padded.resize(MAX_COPIED_BYTES, 0x7f);
    assert_eq!(copied_paths(&padded).unwrap().len(), 1);
    padded.push(0);
    assert_eq!(copied_paths(&padded), Err(Error::InvalidPath));
}
