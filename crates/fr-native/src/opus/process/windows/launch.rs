//! Create one child already assigned to a non-breakaway, kill-on-close job.
//! Only two explicit private handles are inherited. No shell, PATH lookup,
//! ambient environment, late job assignment or suspended unowned child.
use super::{abi_size, handle, image::Image, os, pipe::Pipe};
use std::{
    io,
    marker::PhantomData,
    mem::size_of_val,
    os::windows::io::{FromRawHandle, OwnedHandle},
    path::Path,
};
use windows::{
    Win32::{
        Foundation::{ERROR_INSUFFICIENT_BUFFER, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0},
        Security::SECURITY_ATTRIBUTES,
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        },
        System::{
            JobObjects::{
                CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
                QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
            },
            Threading::{
                CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
                DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
                InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
                PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
                UpdateProcThreadAttribute, WaitForSingleObject,
            },
        },
    },
    core::{PCWSTR, PWSTR, w},
};

pub(in super::super) struct LaunchError {
    pub(in super::super) error: io::Error,
    /// True only if no process was started, or the original job proves empty.
    pub(in super::super) quiescent: bool,
}
impl From<io::Error> for LaunchError {
    fn from(error: io::Error) -> Self {
        Self {
            error,
            quiescent: true,
        }
    }
}

struct Job(OwnedHandle);
impl Job {
    fn new() -> io::Result<Self> {
        // SAFETY: an unnamed, non-inheritable job with no callback pointers.
        let raw = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(os)?;
        // SAFETY: CreateJobObjectW returned a new valid owned handle.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(raw.0) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        limits.ProcessMemoryLimit = 256 * 1024 * 1024;
        let bytes = abi_size::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()?;
        // SAFETY: the complete fixed-size structure remains borrowed until the
        // call returns. Neither breakaway flag is set; descendants cannot exit
        // this job, and the active-process bound prevents a second process.
        unsafe {
            SetInformationJobObject(
                handle(&job.0),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                bytes,
            )
        }
        .map_err(os)?;
        Ok(job)
    }
    fn terminate(&self) {
        // SAFETY: this job contains only the original private codec child.
        let _ = unsafe { TerminateJobObject(handle(&self.0), 70) };
    }
    fn is_empty(&self) -> bool {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let Ok(bytes) = abi_size::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() else {
            return false;
        };
        // SAFETY: a live job and fixed-size writable accounting output.
        unsafe {
            QueryInformationJobObject(
                Some(handle(&self.0)),
                JobObjectBasicAccountingInformation,
                (&raw mut accounting).cast(),
                bytes,
                None,
            )
        }
        .is_ok()
            && accounting.ActiveProcesses == 0
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        self.terminate();
        // Closing our sole job handle also kills its child if termination was
        // not yet observable. Closing is never itself a retirement receipt.
    }
}

#[derive(Clone, Copy)]
#[repr(align(16))]
struct AttributeStorage {
    _bytes: [u8; 16],
}
struct Attributes<'a> {
    raw: LPPROC_THREAD_ATTRIBUTE_LIST,
    _storage: Vec<AttributeStorage>,
    // Windows borrows the attribute values until Delete, not just Update.
    _values: PhantomData<&'a [HANDLE]>,
}
impl<'a> Attributes<'a> {
    fn new(inherited: &'a [HANDLE; 2], jobs: &'a [HANDLE; 1]) -> io::Result<Self> {
        let mut size = 0usize;
        // SAFETY: null first call only queries storage for two attributes.
        let queried = unsafe { InitializeProcThreadAttributeList(None, 2, None, &raw mut size) };
        if queried
            .as_ref()
            .err()
            .is_none_or(|e| e.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult())
            || !(1..=65_536).contains(&size)
        {
            return Err(io::ErrorKind::Other.into());
        }
        let mut storage = vec![AttributeStorage { _bytes: [0; 16] }; size.div_ceil(16)];
        let raw = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast());
        // SAFETY: heap storage is 16-byte aligned, covers the returned size,
        // and is owned by the attribute object through Delete below.
        unsafe { InitializeProcThreadAttributeList(Some(raw), 2, None, &raw mut size) }
            .map_err(os)?;
        let attributes = Self {
            raw,
            _storage: storage,
            _values: PhantomData,
        };
        // SAFETY: both exact-size arrays outlive this list. Handles in the
        // inheritance array are real, inheritable handles, never pseudohandles.
        unsafe {
            UpdateProcThreadAttribute(
                raw,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(inherited.as_ptr().cast()),
                size_of_val(inherited),
                None,
                None,
            )
        }
        .map_err(os)?;
        // SAFETY: creation-time job association is mandatory. Older Windows
        // or an incompatible outer job refuses; no uncontained spawn fallback.
        unsafe {
            UpdateProcThreadAttribute(
                raw,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                Some(jobs.as_ptr().cast()),
                size_of_val(jobs),
                None,
                None,
            )
        }
        .map_err(os)?;
        Ok(attributes)
    }
}
impl Drop for Attributes<'_> {
    fn drop(&mut self) {
        // SAFETY: exactly one initialized list, before its storage and its
        // borrowed handle arrays are dropped. Delete retains no pointers.
        unsafe { DeleteProcThreadAttributeList(self.raw) };
    }
}

pub(in super::super) struct Process {
    child: OwnedHandle,
    job: Job,
    id: u32,
    _image: Image,
}
impl Process {
    pub(in super::super) fn spawn(
        path: &Path,
        live: &impl Fn() -> io::Result<()>,
    ) -> Result<(Self, Pipe), LaunchError> {
        let image = Image::pin(path, live)?;
        let (pipe, child_pipe) = Pipe::pair(live)?;
        live()?;
        let job = Job::new()?;
        let security = SECURITY_ATTRIBUTES {
            nLength: abi_size::<SECURITY_ATTRIBUTES>()?,
            bInheritHandle: true.into(),
            ..Default::default()
        };
        // SAFETY: this write-only NUL handle provides no file/network access.
        // It is the only inherited capability besides the connected pipe.
        let raw = unsafe {
            CreateFileW(
                w!("NUL"),
                GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                Some(&raw const security),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(os)?;
        // SAFETY: the successful CreateFileW transfers this unique handle.
        let stderr = unsafe { OwnedHandle::from_raw_handle(raw.0) };
        let inherited = [handle(&child_pipe), handle(&stderr)];
        let jobs = [handle(&job.0)];
        let attributes = Attributes::new(&inherited, &jobs)?;
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = abi_size::<STARTUPINFOEXW>()?;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = inherited[0];
        startup.StartupInfo.hStdOutput = inherited[0];
        startup.StartupInfo.hStdError = inherited[1];
        startup.lpAttributeList = attributes.raw;
        let mut command: Vec<u16> = format!(
            "\"{}\" --parent-pid {}",
            path.to_str()
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?,
            std::process::id()
        )
        .encode_utf16()
        .chain(Some(0))
        .collect();
        let environment = [0u16; 2];
        let mut information = PROCESS_INFORMATION::default();
        live()?;
        // SAFETY: every pointer targets live, correctly sized storage through
        // this call. lpApplicationName is the inspected absolute image, command
        // arguments are fixed local values, and the environment is double-NUL.
        // EXTENDED_STARTUPINFO makes both the HANDLE_LIST and JOB_LIST apply
        // during creation, before the child can execute any instruction.
        let created = unsafe {
            CreateProcessW(
                PCWSTR(image.name.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                true,
                CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                Some(environment.as_ptr().cast()),
                PCWSTR(image.directory.as_ptr()),
                &raw const startup.StartupInfo,
                &raw mut information,
            )
        };
        if let Err(error) = created {
            job.terminate();
            return Err(LaunchError {
                error: os(error),
                quiescent: job.is_empty(),
            });
        }
        // SAFETY: successful CreateProcessW transfers valid process/thread
        // handles. The child is already in the original parent's job.
        let process = unsafe { OwnedHandle::from_raw_handle(information.hProcess.0) };
        // SAFETY: this is the separately owned initial-thread handle.
        let thread = unsafe { OwnedHandle::from_raw_handle(information.hThread.0) };
        drop(thread);
        // Delete the attribute list before its borrowed arrays/handles leave.
        drop(attributes);
        drop(child_pipe);
        drop(stderr);
        Ok((
            Self {
                child: process,
                job,
                id: information.dwProcessId,
                _image: image,
            },
            pipe,
        ))
    }
    pub(in super::super) fn id(&self) -> u32 {
        self.id
    }
    pub(in super::super) fn stop_and_wait(&self) -> bool {
        self.job.terminate();
        // SAFETY: the original process handle proves identity, even after PID
        // reuse. Cleanup waits only on the dedicated supervisor, for at most
        // one second. An uncertain exit permanently retains its admission slot.
        let waited = unsafe { WaitForSingleObject(handle(&self.child), 1_000) };
        waited == WAIT_OBJECT_0 && self.job.is_empty()
    }
}
