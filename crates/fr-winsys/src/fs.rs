//! Files a local user's configuration may trust: opening without following
//! a final reparse point, who owns an object and what its DACL grants to
//! anyone else (the Windows form of the owner and mode bits frd's Unix stores
//! read with `fstat`), and a rename over a file readers may hold open.
use crate::pipe::os;
use core::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetSecurityInfo, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_INFORMATION_CLASS, TOKEN_OWNER,
    TOKEN_QUERY, TOKEN_USER, TokenOwner, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH,
    FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileRenameInfoEx,
    GetFileInformationByHandle, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    SetFileInformationByHandle,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{HSTRING, PWSTR};

// Access rights (winnt.h). The directory forms share bits with the file
// forms: ADD_FILE is WRITE_DATA, ADD_SUBDIRECTORY is APPEND_DATA.
const DELETE: u32 = 0x0001_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_APPEND_DATA: u32 = 0x0004;
const FILE_WRITE_EA: u32 = 0x0010;
const FILE_DELETE_CHILD: u32 = 0x0040;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
/// Delete, rename or re-permission the object or an entry of it.
const REPLACE: u32 =
    DELETE | WRITE_DAC | WRITE_OWNER | FILE_DELETE_CHILD | GENERIC_ALL | GENERIC_WRITE;
/// Any change to the object's data, attributes or entries.
const WRITE: u32 =
    REPLACE | FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES;
// FILE_RENAME_INFO flags (winbase.h): replace an existing target, even one
// others hold open (they keep the old file).
const RENAME_REPLACE_IF_EXISTS: u32 = 0x1;
const RENAME_POSIX_SEMANTICS: u32 = 0x2;
// ACE types and flags (winnt.h).
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const INHERIT_ONLY_ACE: u8 = 0x08;
// Principals that already hold every right on a Windows system.
const LOCAL_SYSTEM: &str = "S-1-5-18";
const ADMINISTRATORS: &str = "S-1-5-32-544";
const TRUSTED_INSTALLER: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

/// Who owns an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// This process's user, or the owner its token gives new objects
    /// (Administrators, for an elevated administrator).
    Us,
    /// `SYSTEM`, Administrators or `TrustedInstaller`.
    System,
    Other,
}

/// Owner, type and the rights granted to anyone but us and the system
/// principals, read from one open handle.
#[derive(Debug, Clone, Copy)]
pub struct Inspection {
    pub owner: Owner,
    pub directory: bool,
    /// A symbolic link, junction or other reparse point, opened as itself.
    pub reparse: bool,
    pub links: u32,
    pub size: u64,
    others: u32,
}
impl Inspection {
    /// Someone else may delete, rename or re-permission this object, or (a
    /// directory) an entry in it. Adding entries alone is not replacing.
    pub const fn others_may_replace(&self) -> bool {
        self.others & REPLACE != 0
    }
    /// Someone else may change the object or add, remove or rename entries.
    pub const fn others_may_write(&self) -> bool {
        self.others & WRITE != 0
    }
    /// Someone else holds any right at all (reading included).
    pub const fn others_may_access(&self) -> bool {
        self.others & !SYNCHRONIZE != 0
    }
}

/// How `open_no_follow` opens a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Open {
    /// An existing file, read; a writer may replace it meanwhile.
    Read,
    /// An existing file or directory, read; no one may delete or rename it
    /// while the handle lives (a directory on a checked path).
    Pinned,
    /// An existing file, read and written (to flush it).
    Write,
    /// A file read and written, created when missing (a lock file).
    Lock,
}

/// Opens `path` without following a final symbolic link or junction: a
/// reparse point is opened as itself, for `inspect` to refuse. Directories
/// open too (backup semantics; no privilege is used or needed).
pub fn open_no_follow(path: &Path, how: Open) -> io::Result<File> {
    let share = FILE_SHARE_READ.0
        | FILE_SHARE_WRITE.0
        | if how == Open::Pinned {
            0
        } else {
            FILE_SHARE_DELETE.0
        };
    OpenOptions::new()
        .read(true)
        .write(matches!(how, Open::Write | Open::Lock))
        .create(how == Open::Lock)
        .truncate(false)
        .share_mode(share)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
}

/// Renames the file `from` over `to`. A reader holding `to` open (sharing
/// deletion) keeps the old contents and does not block the rename. Returns
/// whether the rename was also flushed to disk; `Err` means nothing moved.
pub fn replace(from: &Path, to: &Path) -> io::Result<bool> {
    let source = OpenOptions::new()
        .access_mode(DELETE | GENERIC_WRITE)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_WRITE_THROUGH.0)
        .open(from)?;
    let name: Vec<u16> = to.as_os_str().encode_wide().collect();
    let name_bytes = name.len() * 2;
    // The header's one-element name array leaves room for a terminator.
    let size = size_of::<FILE_RENAME_INFO>() + name_bytes;
    let mut buffer = vec![0_u64; size.div_ceil(8)];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `buffer` is zeroed, 8-byte aligned and holds the header plus
    // the name; the name is copied into the trailing array in place.
    unsafe {
        (*info).Anonymous.Flags = RENAME_REPLACE_IF_EXISTS | RENAME_POSIX_SEMANTICS;
        (*info).FileNameLength =
            u32::try_from(name_bytes).map_err(|_| io::ErrorKind::InvalidInput)?;
        core::ptr::copy_nonoverlapping(
            name.as_ptr(),
            (&raw mut (*info).FileName).cast::<u16>(),
            name.len(),
        );
    }
    let length = u32::try_from(size).map_err(|_| io::ErrorKind::InvalidInput)?;
    // SAFETY: a handle opened with DELETE and the rename record built above.
    let renamed = unsafe {
        SetFileInformationByHandle(
            HANDLE(source.as_raw_handle()),
            FileRenameInfoEx,
            buffer.as_ptr().cast(),
            length,
        )
    }
    .map_err(|e| os(&e));
    match renamed {
        // The handle now names `to`; flushing it commits the rename too.
        Ok(()) => Ok(source.sync_all().is_ok()),
        // Before Windows 10 1709, or a file system without POSIX renames: the
        // classic rename, which an open reader blocks.
        Err(e) if matches!(e.raw_os_error(), Some(50 | 87)) => {
            drop(source);
            let (from, to) = (HSTRING::from(from), HSTRING::from(to));
            // SAFETY: two NUL-terminated wide paths that outlive the call.
            unsafe {
                MoveFileExW(
                    &from,
                    &to,
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            }
            .map_err(|e| os(&e))?;
            Ok(true)
        }
        Err(e) => Err(e),
    }
}

/// The owner, type, link count, size and others' rights of an open file or
/// directory. The handle needs read access (`READ_CONTROL` among it).
pub fn inspect(file: &File) -> io::Result<Inspection> {
    let handle = HANDLE(file.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: an open handle and an out pointer to a local.
    unsafe { GetFileInformationByHandle(handle, &raw mut info) }.map_err(|e| os(&e))?;
    let us = [token_sid(TokenUser)?, token_sid(TokenOwner)?];
    let (owner, others) = security(handle, &us)?;
    Ok(Inspection {
        owner,
        directory: info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
        reparse: info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0,
        links: info.nNumberOfLinks,
        size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        others,
    })
}

/// Memory the security functions allocated, released with `LocalFree`.
struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: memory a security function allocated for us, freed once.
            unsafe { LocalFree(Some(HLOCAL(self.0))) };
        }
    }
}

/// A SID in its string form (`S-1-5-...`).
fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = PWSTR::null();
    // SAFETY: a valid SID and an out pointer to a local.
    unsafe { ConvertSidToStringSidW(sid, &raw mut text) }.map_err(|e| os(&e))?;
    let _owned = Local(text.0.cast());
    // SAFETY: the NUL-terminated string the call just allocated.
    unsafe { text.to_string() }.map_err(|_| io::ErrorKind::InvalidData.into())
}

/// The user or default-owner SID of this process's token.
fn token_sid(class: TOKEN_INFORMATION_CLASS) -> io::Result<String> {
    let mut raw = HANDLE::default();
    // SAFETY: the current-process pseudo handle and an out pointer to a local.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut raw) }
        .map_err(|e| os(&e))?;
    // SAFETY: the token handle just opened, owned only here.
    let token = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    let token = HANDLE(token.as_raw_handle());
    let mut needed = 0_u32;
    // SAFETY: a size query; it fails with ERROR_INSUFFICIENT_BUFFER by design.
    let _ = unsafe { GetTokenInformation(token, class, None, 0, &raw mut needed) };
    // u64 words keep the SID pointer inside the buffer aligned.
    let mut buffer = vec![0_u64; (needed as usize).div_ceil(8).max(1)];
    let length = u32::try_from(buffer.len() * 8).map_err(|_| io::ErrorKind::InvalidData)?;
    // SAFETY: a writable buffer of `length` bytes and an out pointer.
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(buffer.as_mut_ptr().cast()),
            length,
            &raw mut needed,
        )
    }
    .map_err(|e| os(&e))?;
    let sid = if class == TokenUser {
        // SAFETY: the call filled the buffer with a TOKEN_USER.
        unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    } else {
        // SAFETY: the call filled the buffer with a TOKEN_OWNER.
        unsafe { (*buffer.as_ptr().cast::<TOKEN_OWNER>()).Owner }
    };
    sid_string(sid)
}

/// The owner and the rights the DACL grants to anyone but `us` and the
/// system principals. Inherit-only entries do not apply to the object
/// itself; an absent DACL, or an entry type this does not read, counts as
/// granting everything.
fn security(handle: HANDLE, us: &[String]) -> io::Result<(Owner, u32)> {
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = core::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: an open handle with READ_CONTROL and out pointers to locals.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&raw mut owner),
            None,
            Some(&raw mut dacl),
            None,
            Some(&raw mut descriptor),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status.0.cast_signed()));
    }
    // The owner and DACL pointers point into this one allocation.
    let _descriptor = Local(descriptor.0);
    let trusted = |sid: &str| [LOCAL_SYSTEM, ADMINISTRATORS, TRUSTED_INSTALLER].contains(&sid);
    let owner_sid = sid_string(owner)?;
    let owner = if us.contains(&owner_sid) {
        Owner::Us
    } else if trusted(&owner_sid) {
        Owner::System
    } else {
        Owner::Other
    };
    if dacl.is_null() {
        return Ok((owner, u32::MAX));
    }
    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: the descriptor's DACL and an out buffer of the stated size.
    unsafe {
        GetAclInformation(
            dacl,
            (&raw mut size).cast(),
            u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).expect("small struct"),
            AclSizeInformation,
        )
    }
    .map_err(|e| os(&e))?;
    let mut others = 0_u32;
    for index in 0..size.AceCount {
        let mut entry: *mut c_void = core::ptr::null_mut();
        // SAFETY: an index below the DACL's entry count and an out pointer.
        unsafe { GetAce(dacl, index, &raw mut entry) }.map_err(|e| os(&e))?;
        // SAFETY: every ACE starts with an ACE_HEADER; ACEs are 4-byte aligned.
        let header = unsafe { *entry.cast::<ACE_HEADER>() };
        if header.AceFlags & INHERIT_ONLY_ACE != 0 || header.AceType == ACCESS_DENIED_ACE_TYPE {
            continue;
        }
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
            others = u32::MAX;
            continue;
        }
        let allowed = entry.cast::<ACCESS_ALLOWED_ACE>();
        // SAFETY: an ACCESS_ALLOWED_ACE: its mask, then its SID in place.
        let (mask, sid) = unsafe { ((*allowed).Mask, PSID((&raw mut (*allowed).SidStart).cast())) };
        let grantee = sid_string(sid)?;
        if !us.contains(&grantee) && !trusted(&grantee) {
            others |= mask;
        }
    }
    Ok((owner, others))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A folder of this test's own, removed afterwards (a junction inside is
    /// removed as itself, not followed).
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    impl core::ops::Deref for Scratch {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }
    fn scratch(name: &str) -> Scratch {
        let path = std::env::temp_dir().join(format!("fr-winsys-fs-{}-{name}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Scratch(path)
    }

    #[test]
    fn a_new_private_file_is_ours_and_grants_no_one_else_anything() {
        let dir = scratch("private");
        let path = dir.join("policy.json");
        std::fs::write(&path, b"{}").unwrap();
        let file = inspect(&open_no_follow(&path, Open::Read).unwrap()).unwrap();
        assert_eq!(file.owner, Owner::Us);
        assert!(!file.directory && !file.reparse);
        assert_eq!((file.links, file.size), (1, 2));
        assert!(!file.others_may_access(), "{file:?}");
        let folder = inspect(&open_no_follow(&dir, Open::Pinned).unwrap()).unwrap();
        assert!(folder.directory && folder.owner == Owner::Us, "{folder:?}");
        assert!(!folder.others_may_write(), "{folder:?}");
    }

    #[test]
    fn a_grant_to_everyone_is_seen() {
        let dir = scratch("everyone");
        let path = dir.join("policy.json");
        std::fs::write(&path, b"{}").unwrap();
        // Everyone (S-1-1-0) may read the file and add entries to the folder.
        for (target, rights) in [(path.as_path(), "(R)"), (&*dir, "(AD)")] {
            let status = Command::new("icacls")
                .arg(target)
                .args(["/grant", &format!("*S-1-1-0:{rights}")])
                .output()
                .unwrap()
                .status;
            assert!(status.success());
        }
        let file = inspect(&open_no_follow(&path, Open::Read).unwrap()).unwrap();
        assert!(
            file.others_may_access() && !file.others_may_write(),
            "{file:?}"
        );
        let folder = inspect(&open_no_follow(&dir, Open::Pinned).unwrap()).unwrap();
        assert!(
            folder.others_may_write() && !folder.others_may_replace(),
            "{folder:?}"
        );
    }

    #[test]
    fn a_junction_is_opened_as_itself_and_a_hard_link_is_counted() {
        let dir = scratch("links");
        let target = dir.join("target");
        std::fs::create_dir(&target).unwrap();
        let junction = dir.join("junction");
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap()
            .status;
        assert!(status.success());
        assert!(
            inspect(&open_no_follow(&junction, Open::Pinned).unwrap())
                .unwrap()
                .reparse
        );
        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();
        std::fs::hard_link(&file, dir.join("second")).unwrap();
        assert_eq!(
            inspect(&open_no_follow(&file, Open::Read).unwrap())
                .unwrap()
                .links,
            2
        );
    }

    #[test]
    fn replace_renames_over_an_existing_file() {
        let dir = scratch("replace");
        let (from, to) = (dir.join("new"), dir.join("old"));
        std::fs::write(&from, b"new").unwrap();
        std::fs::write(&to, b"old").unwrap();
        // A reader that allows deletion does not block the replacement.
        let mut reader = open_no_follow(&to, Open::Read).unwrap();
        assert!(replace(&from, &to).unwrap());
        // The reader keeps the old file; the name now holds the new one.
        let mut old = String::new();
        std::io::Read::read_to_string(&mut reader, &mut old).unwrap();
        assert_eq!(old, "old");
        drop(reader);
        assert_eq!(std::fs::read(&to).unwrap(), b"new");
        assert!(!from.exists());
    }
}
