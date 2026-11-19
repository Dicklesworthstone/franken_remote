//! Windows regular-file receive custody. Local startup selects the root one
//! component at a time. The disk worker's subsequent effects use the retained
//! root and staging handles. No received path is opened or executed.
use super::{Local, Owner, inspect, token_sid};
use crate::file_selection_policy::{MAX_PATH_UNITS, ordinary_absolute, ordinary_component};
use crate::pipe::os;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle};
use std::path::Path;
use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation,
    NTCREATEFILE_CREATE_DISPOSITION, NTCREATEFILE_CREATE_OPTIONS, NtCreateFile,
    NtSetInformationFile,
};
use windows::Win32::Foundation::{
    HANDLE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_DESCRIPTOR, TokenUser};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_TRAVERSE, FILE_TYPE_DISK, FileDispositionInfo, GetDriveTypeW, GetFileInformationByHandle,
    GetFileType, SetFileInformationByHandle,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;
use windows::core::{HSTRING, PCWSTR, PWSTR};

/// A locally selected receiving root failed lexical or actual-handle admission.
#[derive(Debug)]
pub enum ReceiveRootError {
    Path,
    Unsafe,
    Io(io::Error),
}
impl From<io::Error> for ReceiveRootError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

// Directory anchors retain traverse, metadata and synchronization rights.
// READ_CONTROL permits owner/DACL inspection. NtCreateFile and rename check
// write permission at the effect; retained directory write access is unnecessary.
const DIRECTORY_ACCESS: FILE_ACCESS_RIGHTS =
    FILE_ACCESS_RIGHTS(0x0002_0000 | 0x0010_0000 | FILE_READ_ATTRIBUTES.0 | FILE_TRAVERSE.0);

/// Open an existing ordinary absolute local-drive directory. Every component
/// is opened relative to the preceding retained directory, as the reparse
/// object itself, and checked before it can become the next anchor. The final
/// handle is owned by this user/default owner and grants no other untrusted
/// principal write access. Delete sharing is denied for the retained root.
///
/// The local desktop user, administrators, the OS and filesystem remain trusted.
/// No pathname is consulted again after this selection. Root metadata durability
/// is not inferred from this function or from a successful later rename.
pub fn open_receive_root(path: &Path) -> Result<File, ReceiveRootError> {
    let units: Vec<_> = path
        .as_os_str()
        .encode_wide()
        .take(MAX_PATH_UNITS + 1)
        .collect();
    if !ordinary_absolute(&units) {
        return Err(ReceiveRootError::Path);
    }
    let drive = [units[0], u16::from(b':'), u16::from(b'\\'), 0];
    // SAFETY: a bounded, validated, NUL-terminated drive root remains alive.
    // Refuse remote/unknown drive mappings before opening their files.
    if !matches!(unsafe { GetDriveTypeW(PCWSTR(drive.as_ptr())) }, 2 | 3 | 6) {
        return Err(ReceiveRootError::Path);
    }
    let mut root_name: Vec<_> = r"\??\".encode_utf16().collect();
    root_name.extend_from_slice(&drive[..3]);
    let mut directory = open_native(
        HANDLE::default(),
        &mut root_name,
        DIRECTORY_ACCESS,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        None,
    )?;
    check_directory(&directory)?;
    for component in units[3..].split(|unit| matches!(*unit, 0x2f | 0x5c)) {
        let mut component = component.to_vec();
        directory = open_native(
            HANDLE(directory.as_raw_handle()),
            &mut component,
            DIRECTORY_ACCESS,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            None,
        )?;
        check_directory(&directory)?;
    }
    let actual = inspect(&directory)?;
    if actual.owner != Owner::Us || actual.others_may_write() {
        return Err(ReceiveRootError::Unsafe);
    }
    Ok(directory)
}

fn check_directory(directory: &File) -> Result<(), ReceiveRootError> {
    let handle = HANDLE(directory.as_raw_handle());
    // SAFETY: the File owns the handle throughout this query.
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK {
        return Err(ReceiveRootError::Path);
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: a live borrowed handle and a complete writable result.
    unsafe { GetFileInformationByHandle(handle, &raw mut info) }.map_err(|e| os(&e))?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(ReceiveRootError::Unsafe);
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
        return Err(io::Error::from(io::ErrorKind::NotADirectory).into());
    }
    Ok(())
}

fn component(name: &str) -> io::Result<Vec<u16>> {
    if name.len() > 255 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let units: Vec<_> = name.encode_utf16().collect();
    if !ordinary_component(&units) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(units)
}

fn private_security() -> io::Result<Local> {
    let user = token_sid(TokenUser)?;
    // Protected DACL: inheritance cannot make a partial or published file
    // readable by a principal outside this user and the trusted system roles.
    let sddl = HSTRING::from(format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{user})"));
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the SDDL contains only a SID returned by the token API and fixed
    // ACE syntax. The out allocation is owned by Local and freed exactly once.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &sddl,
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    }
    .map_err(|e| os(&e))?;
    Ok(Local(descriptor.0))
}

fn open_native(
    root: HANDLE,
    name: &mut [u16],
    access: FILE_ACCESS_RIGHTS,
    sharing: FILE_SHARE_MODE,
    disposition: NTCREATEFILE_CREATE_DISPOSITION,
    options: NTCREATEFILE_CREATE_OPTIONS,
    security: Option<&Local>,
) -> io::Result<File> {
    let bytes = name
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|bytes| u16::try_from(bytes).ok())
        .ok_or(io::ErrorKind::InvalidInput)?;
    let name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: PWSTR(name.as_mut_ptr()),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).expect("fixed native header"),
        RootDirectory: root,
        ObjectName: &raw const name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: security.map_or(std::ptr::null(), |security| {
            security.0.cast::<SECURITY_DESCRIPTOR>().cast_const()
        }),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut status_block = IO_STATUS_BLOCK::default();
    let mut handle = HANDLE::default();
    // SAFETY: all name/security storage and the borrowed root outlive this
    // synchronous open. Output pointers name initialized writable objects.
    // Inheritance is not requested. FILE_OPEN_REPARSE_POINT prevents following
    // the single component; callers inspect existing directory handles before
    // using them as the next root. New regular files use exclusive FILE_CREATE.
    let status = unsafe {
        NtCreateFile(
            &raw mut handle,
            access,
            &raw const attributes,
            &raw mut status_block,
            None,
            FILE_ATTRIBUTE_NORMAL,
            sharing,
            disposition,
            options,
            None,
            0,
        )
    };
    if status.is_err() {
        // SAFETY: translates a returned status value; it retains no pointers.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code.cast_signed()));
    }
    if handle.is_invalid() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    // SAFETY: this successful synchronous open returned one unique owned file
    // handle. File closes it once, including every later error/unwind path.
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

/// Create a new private regular file relative to the already selected root.
/// Existing objects, including reparse points, are never opened or overwritten.
/// Data and delete access are exclusive; the protected DACL excludes untrusted
/// readers. The selected local user, administrators and OS remain trusted.
pub fn create_receive_file(directory: BorrowedHandle<'_>, name: &str) -> io::Result<File> {
    let mut name = component(name)?;
    let security = private_security()?;
    open_native(
        HANDLE(directory.as_raw_handle()),
        &mut name,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE,
        FILE_SHARE_MODE(0),
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        Some(&security),
    )
}

/// One bounded rename record, prepared before the final authority callback.
/// Both original handles stay borrowed for its complete lifetime.
pub struct PreparedReceiveRename<'a> {
    source: BorrowedHandle<'a>,
    _directory: BorrowedHandle<'a>,
    buffer: Vec<u64>,
    bytes: u32,
}

/// Prepare the checked, NUL-terminated single-component destination and its
/// aligned native record. This performs no filesystem operation or native query.
pub fn prepare_receive_rename<'a>(
    source: BorrowedHandle<'a>,
    directory: BorrowedHandle<'a>,
    name: &str,
) -> io::Result<PreparedReceiveRename<'a>> {
    let name = component(name)?;
    let name_bytes = name
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or(io::ErrorKind::InvalidInput)?;
    // The native contract requires the complete structure plus the name's
    // bytes, including any trailing structure padding. The existing FileName
    // slot also leaves room for the zero terminator beyond the copied name.
    let bytes = size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(name_bytes)
        .ok_or(io::ErrorKind::InvalidInput)?;
    let mut buffer = Vec::<u64>::new();
    buffer
        .try_reserve_exact(bytes.div_ceil(size_of::<u64>()))
        .map_err(|_| io::ErrorKind::OutOfMemory)?;
    buffer.resize(bytes.div_ceil(size_of::<u64>()), 0);
    let record = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: the zeroed buffer is aligned for FILE_RENAME_INFORMATION and
    // contains its header, the checked name and one NUL. The stored handles stay
    // borrowed in PreparedReceiveRename. No untrusted pointer enters the record.
    unsafe {
        (*record).Anonymous.ReplaceIfExists = false;
        (*record).RootDirectory = HANDLE(directory.as_raw_handle());
        (*record).FileNameLength =
            u32::try_from(name_bytes).map_err(|_| io::ErrorKind::InvalidInput)?;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            (&raw mut (*record).FileName).cast::<u16>(),
            name.len(),
        );
    }
    Ok(PreparedReceiveRename {
        source,
        _directory: directory,
        buffer,
        bytes: u32::try_from(bytes).map_err(|_| io::ErrorKind::InvalidInput)?,
    })
}
impl PreparedReceiveRename<'_> {
    /// The single no-replace external effect. Call immediately after the
    /// original authority check; no name lookup, reopen, probe or allocation
    /// precedes it here. Success means published, not proven crash-durable.
    pub fn commit(self) -> io::Result<()> {
        // The NT call keeps the name relative to the retained root handle.
        // SetFileInformationByHandle(FileRenameInfo) expands it to a full DOS
        // path from the process's current directory, which is invalid beside
        // RootDirectory: every publication failed ERROR_INVALID_PARAMETER.
        let mut status_block = IO_STATUS_BLOCK::default();
        // SAFETY: the prepared aligned record, the status block and both
        // borrowed original handles remain live for this synchronous call (the
        // staging handle was opened FILE_SYNCHRONOUS_IO_NONALERT).
        // ReplaceIfExists is false.
        let status = unsafe {
            NtSetInformationFile(
                HANDLE(self.source.as_raw_handle()),
                &raw mut status_block,
                self.buffer.as_ptr().cast(),
                self.bytes,
                FileRenameInformation,
            )
        };
        if status.is_err() {
            // SAFETY: translates a returned status value; it retains no pointers.
            let code = unsafe { RtlNtStatusToDosError(status) };
            return Err(io::Error::from_raw_os_error(code.cast_signed()));
        }
        Ok(())
    }
}

/// Mark only the retained unpublished file for deletion. The caller must close
/// its final handle before returning reservation credit. No pathname is reopened.
/// Never call after successful publication.
pub fn remove_receive_file(file: BorrowedHandle<'_>) -> io::Result<()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: this live borrowed file handle and complete record outlive the
    // synchronous call. The staging open requested DELETE access.
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&raw const disposition).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).expect("fixed native record"),
        )
    }
    .map_err(|e| os(&e))
}
