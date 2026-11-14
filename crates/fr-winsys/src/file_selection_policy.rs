//! Admission before a Windows upload can open a native path. This is the
//! ordinary drive-path subset; explicit device/share namespaces, streams and
//! DOS device names are refused before open. Ancestor junction resolution is
//! still an OS path operation, not a claim of physically local storage.
#![forbid(unsafe_code)]

pub(super) const MAX_PATH_UNITS: usize = 32_760;

fn separator(unit: u16) -> bool {
    unit == u16::from(b'\\') || unit == u16::from(b'/')
}

fn reserved(component: &[u16]) -> bool {
    let base = component
        .split(|unit| *unit == u16::from(b'.'))
        .next()
        .unwrap_or_default();
    let matches = |name: &[u8]| {
        base.len() == name.len()
            && base.iter().zip(name).all(|(unit, ascii)| {
                u8::try_from(*unit).is_ok_and(|unit| unit.eq_ignore_ascii_case(ascii))
            })
    };
    if [
        b"CON".as_slice(),
        b"PRN",
        b"AUX",
        b"NUL",
        b"CONIN$",
        b"CONOUT$",
        b"CLOCK$",
    ]
    .into_iter()
    .any(matches)
    {
        return true;
    }
    base.len() == 4
        && (1..=9).any(|digit| {
            let last = base[3];
            let suffix = last == u16::from(b'0') + digit
                || matches!((digit, last), (1, 0x00b9) | (2, 0x00b2) | (3, 0x00b3));
            suffix
                && [b"COM".as_slice(), b"LPT"].into_iter().any(|prefix| {
                    base[..3].iter().zip(prefix).all(|(unit, ascii)| {
                        u8::try_from(*unit).is_ok_and(|unit| unit.eq_ignore_ascii_case(ascii))
                    })
                })
        })
}

pub(super) fn ordinary_absolute(path: &[u16]) -> bool {
    if !(4..=MAX_PATH_UNITS).contains(&path.len())
        || !u8::try_from(path[0]).is_ok_and(|unit| unit.is_ascii_alphabetic())
        || path[1] != u16::from(b':')
        || !separator(path[2])
    {
        return false;
    }
    path[3..].split(|unit| separator(*unit)).all(|component| {
        !component.is_empty()
            && !component
                .last()
                .is_some_and(|unit| matches!(*unit, 0x20 | 0x2e))
            && !component.iter().any(|unit| {
                *unit < 0x20 || matches!(*unit, 0x22 | 0x2a | 0x3a | 0x3c | 0x3e | 0x3f | 0x7c)
            })
            && !reserved(component)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(path: &str) -> bool {
        ordinary_absolute(&path.encode_utf16().collect::<Vec<_>>())
    }

    #[test]
    fn ordinary_local_paths_preserve_unicode_and_portable_spaces() {
        assert!(allowed("C:\\Users\\Local User\\漢字🙂.txt"));
        assert!(allowed("d:/chosen/file.txt"));
        assert!(allowed("C:\\.config\\normal"));
    }

    #[test]
    fn namespaces_and_unqualified_paths_never_reach_open() {
        for path in [
            "file.txt",
            "C:file.txt",
            "\\file.txt",
            "C:\\",
            "\\\\server\\share\\file",
            "\\\\.\\pipe\\name",
            "\\\\?\\GLOBALROOT\\Device\\HarddiskVolume1\\file",
            "\\\\?\\C:\\file",
            "//server/share/file",
            "C:\\file:stream",
            "C:\\a\\..\\file",
            "C:\\a\\.\\file",
            "C:\\a\\\\file",
            "C:\\a\\file ",
            "C:\\a\\file.",
            "C:\\a\\file\0suffix",
            "C:\\a\\f?le",
        ] {
            assert!(!allowed(path), "{path:?}");
        }
    }

    #[test]
    fn dos_devices_are_refused_in_every_component_and_with_extensions() {
        for name in [
            "CON",
            "con.txt",
            "NUL",
            "aux.log",
            "PRN",
            "COM1",
            "com9.txt",
            "LPT1",
            "LPT9",
            "COM¹",
            "LPT².log",
            "CONIN$",
            "CONOUT$",
            "CLOCK$",
        ] {
            assert!(!allowed(&format!("C:\\{name}\\file.txt")), "{name}");
            assert!(!allowed(&format!("C:\\chosen\\{name}")), "{name}");
        }
        for name in ["console", "company.txt", "COM10", "LPT0", "auxiliary"] {
            assert!(allowed(&format!("C:\\chosen\\{name}")));
        }
    }

    #[test]
    fn native_path_storage_is_bounded_before_allocation_or_open() {
        let mut path: Vec<_> = "C:\\".encode_utf16().collect();
        path.resize(MAX_PATH_UNITS, u16::from(b'a'));
        assert!(ordinary_absolute(&path));
        path.push(u16::from(b'a'));
        assert!(!ordinary_absolute(&path));
    }
}
