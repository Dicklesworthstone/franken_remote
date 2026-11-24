//! Authenticate the inherited endpoint and parent before any codec input.
//! The job is a lifetime/resource boundary, explicitly not an OS sandbox.
#[path = "../../windows_capture/parent.rs"]
mod parent;

use super::AudioMediaError as Error;
use std::os::windows::io::AsRawHandle;
use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType},
    System::{
        JobObjects::{
            JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, QueryInformationJobObject,
        },
        Pipes::GetNamedPipeServerProcessId,
    },
};

pub(super) fn prepare(expected: u32) -> Result<(), Error> {
    if expected == 0 || expected == std::process::id() {
        return Err(Error::Fatal);
    }
    let (input, output) = (std::io::stdin(), std::io::stdout());
    for raw in [input.as_raw_handle(), output.as_raw_handle()] {
        let handle = HANDLE(raw);
        // SAFETY: borrowed process standard handles remain live for this check.
        if handle.is_invalid() || unsafe { GetFileType(handle) } != FILE_TYPE_PIPE {
            return Err(Error::Fatal);
        }
        let mut server = 0u32;
        // SAFETY: a verified pipe handle and a live output. The kernel's server
        // PID binds the inherited endpoint to the command-line parent, rather
        // than trusting a caller-provided pipe name or a handle-shaped number.
        unsafe { GetNamedPipeServerProcessId(handle, &raw mut server) }
            .map_err(|_| Error::Fatal)?;
        if server != expected {
            return Err(Error::Fatal);
        }
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let bytes = super::windows::abi_size::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()
        .map_err(|_| Error::Fatal)?;
    // SAFETY: a null job asks for this process's immediate containing job. A
    // fixed-size output is borrowed only until this query returns.
    unsafe {
        QueryInformationJobObject(
            None,
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            bytes,
            None,
        )
    }
    .map_err(|_| Error::Fatal)?;
    let required = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    if !limits.BasicLimitInformation.LimitFlags.contains(required)
        || limits.BasicLimitInformation.ActiveProcessLimit != 1
        || limits.ProcessMemoryLimit == 0
        || limits.ProcessMemoryLimit > 256 * 1024 * 1024
    {
        return Err(Error::Fatal);
    }
    // This helper opens the original parent, checks creation times against PID
    // reuse, then retains that exact handle in a process-exit watcher thread.
    // Its narrow source has no dependency on the hardware-media capture module.
    parent::bind_parent(expected).map_err(|_| Error::Fatal)
}
