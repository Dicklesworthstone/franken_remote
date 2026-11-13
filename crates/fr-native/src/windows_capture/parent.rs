//! The Windows form of Linux's parent-death signal: a worker ends when the
//! frd that started it does.
use crate::NativeError;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows::Win32::Foundation::{FILETIME, HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, INFINITE, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

/// When `process` started, in 100 ns units since 1601.
fn started(process: HANDLE) -> Result<u64, NativeError> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: a process handle with query rights and four out values.
    unsafe {
        GetProcessTimes(
            process,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    }
    .map_err(|_| NativeError::Unavailable)?;
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// Ends this process when `expected` exits. Refuses a process that has
/// already exited, and one started after this process (its id was reused:
/// the parent is gone).
pub fn bind_parent(expected: u32) -> Result<(), NativeError> {
    // SAFETY: opens a process by id for waiting and time queries only.
    let raw = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            expected,
        )
    }
    .map_err(|_| NativeError::Unavailable)?;
    // SAFETY: the new process handle, owned only here.
    let parent = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    let handle = HANDLE(parent.as_raw_handle());
    // SAFETY: the current-process pseudo handle needs no closing.
    if started(handle)? > started(unsafe { GetCurrentProcess() })? {
        return Err(NativeError::InvalidConfiguration);
    }
    // SAFETY: a zero-timeout wait on an owned process handle.
    if unsafe { WaitForSingleObject(handle, 0) } != WAIT_TIMEOUT {
        return Err(NativeError::Closed);
    }
    std::thread::Builder::new()
        .name("parent-exit".into())
        .spawn(move || {
            // SAFETY: an owned process handle, kept open by this thread.
            unsafe { WaitForSingleObject(HANDLE(parent.as_raw_handle()), INFINITE) };
            std::process::exit(1);
        })
        .map(drop)
        .map_err(|_| NativeError::Allocation)
}
