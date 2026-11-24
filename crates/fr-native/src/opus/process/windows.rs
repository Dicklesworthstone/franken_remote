//! Narrow Win32 boundary for the private Opus worker. Job limits and a strict
//! inherited-handle list provide process containment, not an OS sandbox.
mod image;
mod launch;
mod pipe;
mod security;

pub(super) use launch::{LaunchError, Process};
pub(super) use pipe::Pipe;

use ::windows::Win32::Foundation::HANDLE;
use std::{
    io,
    os::windows::io::{AsRawHandle, OwnedHandle},
};

fn handle(owned: &OwnedHandle) -> HANDLE {
    HANDLE(owned.as_raw_handle())
}
pub(super) fn abi_size<T>() -> io::Result<u32> {
    u32::try_from(std::mem::size_of::<T>()).map_err(|_| io::ErrorKind::InvalidInput.into())
}
// Result::map_err transfers its native error; callers never retain it.
#[allow(clippy::needless_pass_by_value)]
fn os(error: ::windows::core::Error) -> io::Error {
    let code = error.code().0.cast_unsigned();
    if code & 0xffff_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((code & 0xffff).cast_signed())
    } else {
        io::Error::other("private Opus worker native operation failed")
    }
}
