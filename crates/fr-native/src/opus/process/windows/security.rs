//! A private pipe grants access only to the original interactive logon.
use super::{abi_size, handle, os};
use std::{
    ffi::c_void,
    io,
    mem::size_of,
    os::windows::io::{FromRawHandle, OwnedHandle},
};
use windows::{
    Win32::{
        Foundation::{ERROR_INSUFFICIENT_BUFFER, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            },
            GetTokenInformation, IsValidSid, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
            TOKEN_GROUPS, TOKEN_QUERY, TokenLogonSid,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::{PCWSTR, PWSTR},
};

struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        // SAFETY: each non-null allocation came from a documented LocalAlloc
        // output, is owned only here, and no borrowed descriptor outlives it.
        unsafe { LocalFree(Some(HLOCAL(self.0))) };
    }
}
pub(super) struct Descriptor(Local);
impl Descriptor {
    pub(super) fn for_logon() -> io::Result<Self> {
        let mut raw = HANDLE::default();
        // SAFETY: a pseudo process handle and a live out handle. Query rights
        // cannot change the current token or impersonate another principal.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut raw) }.map_err(os)?;
        // SAFETY: successful OpenProcessToken transfers exactly this handle.
        let token = unsafe { OwnedHandle::from_raw_handle(raw.0) };
        let mut size = 0u32;
        // SAFETY: this null-buffer call only asks for the required byte count.
        let queried =
            unsafe { GetTokenInformation(handle(&token), TokenLogonSid, None, 0, &raw mut size) };
        let minimum = abi_size::<TOKEN_GROUPS>()?;
        if queried
            .as_ref()
            .err()
            .is_none_or(|e| e.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult())
            || !(minimum..=65_536).contains(&size)
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let mut data = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        let mut used = size;
        // SAFETY: the aligned allocation covers at least `size` bytes and
        // remains alive through every use of the returned SID pointers.
        unsafe {
            GetTokenInformation(
                handle(&token),
                TokenLogonSid,
                Some(data.as_mut_ptr().cast()),
                size,
                &raw mut used,
            )
        }
        .map_err(os)?;
        if used > size || (used as usize) < size_of::<TOKEN_GROUPS>() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // SAFETY: TOKEN_GROUPS is correctly aligned and fully present above.
        let groups = unsafe { &*data.as_ptr().cast::<TOKEN_GROUPS>() };
        if groups.GroupCount != 1 || groups.Groups[0].Attributes & 0xc000_0000 != 0xc000_0000 {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let sid = groups.Groups[0].Sid;
        let start = data.as_ptr() as usize;
        let offset = (sid.0 as usize)
            .checked_sub(start)
            .ok_or(io::ErrorKind::InvalidData)?;
        if offset.checked_add(8).is_none_or(|end| end > used as usize) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // SAFETY: the SID's fixed eight-byte prefix is inside the returned
        // buffer. Check its variable length before asking Windows to read it.
        let count = unsafe { *data.as_ptr().cast::<u8>().add(offset + 1) } as usize;
        if count > 15
            || offset
                .checked_add(8 + 4 * count)
                .is_none_or(|end| end > used as usize)
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // SAFETY: the complete SID lies inside the token-information buffer.
        if !unsafe { IsValidSid(sid) }.as_bool() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut text = PWSTR::null();
        // SAFETY: the validated SID and live out pointer; Windows allocates a
        // NUL-terminated string freed by its immediate Local owner below.
        unsafe { ConvertSidToStringSidW(sid, &raw mut text) }.map_err(os)?;
        let _text_owner = Local(text.0.cast());
        // SAFETY: successful ConvertSidToStringSidW guarantees this string's
        // termination. It remains allocated until after its Rust copy.
        let sid = unsafe { text.to_string() }.map_err(|_| io::ErrorKind::InvalidData)?;
        if !sid.starts_with("S-1-5-5-") || sid.len() > 184 {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut raw = PSECURITY_DESCRIPTOR::default();
        // SAFETY: a bounded NUL-terminated SDDL string and a live out pointer.
        // SDDL_REVISION_1 is 1. The resulting descriptor is held through all
        // calls that borrow its SECURITY_ATTRIBUTES, then LocalFree'd.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                1,
                &raw mut raw,
                None,
            )
        }
        .map_err(os)?;
        if raw.is_invalid() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(Self(Local(raw.0)))
    }
    pub(super) fn attributes(&self, inherit: bool) -> io::Result<SECURITY_ATTRIBUTES> {
        Ok(SECURITY_ATTRIBUTES {
            nLength: abi_size::<SECURITY_ATTRIBUTES>()?,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: inherit.into(),
        })
    }
}
