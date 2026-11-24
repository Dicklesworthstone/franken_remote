//! Pin the exact local executable and every ancestor before `CreateProcessW`.
use super::super::windows_policy;
use fr_winsys::fs::{self, Open, Owner};
use std::{
    fs::{File, OpenOptions},
    io,
    os::windows::fs::OpenOptionsExt,
    path::Path,
};
use windows::{
    Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, GetDriveTypeW},
    core::PCWSTR,
};

pub(super) struct Image {
    pub(super) name: Vec<u16>,
    pub(super) directory: Vec<u16>,
    // Open handles deny removal/renaming through all inspected ancestors. The
    // executable also denies concurrent writes, including by our own account.
    _pins: Vec<File>,
}
impl Image {
    pub(super) fn pin(path: &Path, live: &impl Fn() -> io::Result<()>) -> io::Result<Self> {
        let name = path.to_str().ok_or(io::ErrorKind::InvalidInput)?;
        let root = windows_policy::image_root(name)?;
        let leaf = name.rfind('\\').ok_or(io::ErrorKind::InvalidInput)?;
        let drive: Vec<_> = name[root - 3..root].encode_utf16().chain(Some(0)).collect();
        live()?;
        // SAFETY: a validated, NUL-terminated drive root. Only a local fixed
        // volume may contain this installed executable; mapped drives refuse.
        if unsafe { GetDriveTypeW(PCWSTR(drive.as_ptr())) } != 3 {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let mut pins = Vec::new();
        let boundaries = std::iter::once(root).chain(
            name[root..]
                .match_indices('\\')
                .map(|(offset, _)| root + offset),
        );
        for end in boundaries {
            live()?;
            let file = fs::open_no_follow(Path::new(&name[..end]), Open::Pinned)?;
            let info = fs::inspect(&file)?;
            if !info.directory
                || info.reparse
                || info.owner == Owner::Other
                || info.others_may_replace()
                || (end == leaf.max(root) && info.others_may_write())
            {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            pins.push(file);
        }
        live()?;
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)?;
        let info = fs::inspect(&file)?;
        if info.directory
            || info.reparse
            || info.owner == Owner::Other
            || info.others_may_write()
            || info.links != 1
            || info.size == 0
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        pins.push(file);
        live()?;
        Ok(Self {
            name: name.encode_utf16().chain(Some(0)).collect(),
            directory: name[..leaf.max(root)]
                .encode_utf16()
                .chain(Some(0))
                .collect(),
            _pins: pins,
        })
    }
}
