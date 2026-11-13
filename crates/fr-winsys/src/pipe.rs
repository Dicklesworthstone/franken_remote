//! The client end of a local named pipe, read and written without blocking.
//!
//! Overlapped I/O behind poll functions, so a runtime's stream traits can sit
//! on top (fr-tailnet wraps this in Asupersync's `AsyncRead`/`AsyncWrite`).
//! The system thread pool watches each operation's completion event
//! (`RegisterWaitForSingleObject`) and its callback wakes the task: no thread
//! blocks on the pipe and nothing polls in a loop.
//!
//! Every operation owns its buffer, `OVERLAPPED` and event on the heap.
//! Dropping the client with an operation unfinished cancels it and waits
//! for the cancellation (prompt for a pipe) before anything is released, so
//! the kernel never writes into freed memory and dropping a future at a
//! caller's deadline ends an exchange cleanly.
use core::ffi::c_void;
use core::task::{Context, Poll, Waker};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::Mutex;
use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_MORE_DATA,
    HANDLE, INVALID_HANDLE_VALUE, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_OVERLAPPED, ReadFile, SECURITY_IDENTIFICATION, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, RegisterWaitForSingleObject, UnregisterWaitEx, WT_EXECUTEONLYONCE,
};

/// Bytes asked of the kernel per read.
const READ_CHUNK: usize = 16 * 1024;
const PIPE_NAMESPACE: &str = r"\\.\pipe\";

/// A connected pipe client and the process that serves it.
pub struct PipeClient {
    file: File,
    server: u32,
    /// The read in flight, if any.
    reading: Option<Operation>,
    /// A completed read's bytes not yet handed out, from `unread_at`.
    unread: Vec<u8>,
    unread_at: usize,
    /// The server closed its end.
    ended: bool,
    /// The write in flight: `poll_write` reports bytes accepted once the
    /// write starts, and the next write or `poll_flush` waits for it.
    writing: Option<Operation>,
}

/// The Win32 error inside a `windows` error, so `io::ErrorKind` and the raw
/// code mean what they do for std's own calls (`windows` keeps the whole
/// HRESULT).
fn os(error: &windows::core::Error) -> io::Error {
    let code = error.code().0.cast_unsigned();
    if code & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((code & 0xFFFF).cast_signed())
    } else {
        io::Error::other("windows error")
    }
}

fn is_win32(error: &windows::core::Error, code: WIN32_ERROR) -> bool {
    error.code() == code.to_hresult()
}

/// The server closed its end: a pipe's end of file.
fn is_end(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        code == ERROR_BROKEN_PIPE.0.cast_signed() || code == ERROR_HANDLE_EOF.0.cast_signed()
    })
}

impl PipeClient {
    /// Connects to `path` (`\\.\pipe\...`) for reading and writing. The
    /// server may identify this client (learn who is asking) but not
    /// impersonate it. A path outside the local pipe namespace is refused.
    pub fn open(path: &Path) -> io::Result<Self> {
        let text = path.to_str().ok_or(io::ErrorKind::InvalidInput)?;
        if !text.starts_with(PIPE_NAMESPACE) || text.len() <= PIPE_NAMESPACE.len() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .security_qos_flags(SECURITY_IDENTIFICATION.0)
            .custom_flags(FILE_FLAG_OVERLAPPED.0)
            .open(path)?;
        let mut server = 0_u32;
        // SAFETY: a live pipe handle and an out pointer to a local.
        unsafe { GetNamedPipeServerProcessId(HANDLE(file.as_raw_handle()), &raw mut server) }
            .map_err(|e| os(&e))?;
        Ok(Self {
            file,
            server,
            reading: None,
            unread: Vec::new(),
            unread_at: 0,
            ended: false,
            writing: None,
        })
    }

    /// The process ID of the pipe's server, as the kernel reports it.
    pub const fn server_process_id(&self) -> u32 {
        self.server
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.file.as_raw_handle())
    }

    /// Reads into `out`; `Ok(0)` once the server has closed its end.
    pub fn poll_read(&mut self, cx: &Context<'_>, out: &mut [u8]) -> Poll<io::Result<usize>> {
        if out.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if self.unread_at < self.unread.len() {
            let count = out.len().min(self.unread.len() - self.unread_at);
            out[..count].copy_from_slice(&self.unread[self.unread_at..self.unread_at + count]);
            self.unread_at += count;
            return Poll::Ready(Ok(count));
        }
        if self.ended {
            return Poll::Ready(Ok(0));
        }
        if self.reading.is_none() {
            match Operation::start(self.handle(), vec![0; READ_CHUNK], Direction::Read, cx) {
                Ok(operation) => self.reading = Some(operation),
                Err(error) if is_end(&error) => {
                    self.ended = true;
                    return Poll::Ready(Ok(0));
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
        let Some(operation) = self.reading.as_mut() else {
            return Poll::Ready(Err(io::ErrorKind::Other.into()));
        };
        let result = match operation.poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result,
        };
        let finished = self.reading.take();
        match result {
            // A zero-byte message: read again on the next poll instead of
            // spinning inside this one.
            Ok(0) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Ok(read) => {
                let mut buffer = finished.map(Operation::into_buffer).unwrap_or_default();
                buffer.truncate(read);
                self.unread = buffer;
                self.unread_at = 0;
                self.poll_read(cx, out)
            }
            Err(error) if is_end(&error) => {
                self.ended = true;
                Poll::Ready(Ok(0))
            }
            Err(error) => Poll::Ready(Err(error)),
        }
    }

    /// Starts writing `bytes` and reports them all accepted; a write still
    /// in flight is waited for first, and an error it ended with is
    /// reported here or by [`Self::poll_flush`].
    pub fn poll_write(&mut self, cx: &Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if let Err(error) = core::task::ready!(self.poll_flush(cx)) {
            return Poll::Ready(Err(error));
        }
        match Operation::start(self.handle(), bytes.to_vec(), Direction::Write, cx) {
            Ok(operation) => {
                self.writing = Some(operation);
                Poll::Ready(Ok(bytes.len()))
            }
            Err(error) => Poll::Ready(Err(error)),
        }
    }

    /// Waits for the write in flight, if any.
    pub fn poll_flush(&mut self, cx: &Context<'_>) -> Poll<io::Result<()>> {
        let Some(operation) = self.writing.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        let expected = operation.buffer.len();
        let result = core::task::ready!(operation.poll(cx));
        self.writing = None;
        Poll::Ready(match result {
            Ok(written) if written == expected => Ok(()),
            Ok(_) => Err(io::ErrorKind::WriteZero.into()),
            Err(error) => Err(error),
        })
    }
}

impl Drop for PipeClient {
    fn drop(&mut self) {
        // Unfinished operations are cancelled and drained while the handle
        // they name is still open.
        self.reading = None;
        self.writing = None;
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Read,
    Write,
}

/// What the thread-pool callback reaches: the waker to wake.
struct Signal {
    waker: Mutex<Option<Waker>>,
}
impl Signal {
    fn set(&self, waker: &Waker) {
        if let Ok(mut slot) = self.waker.lock() {
            match slot.as_ref() {
                Some(current) if current.will_wake(waker) => {}
                _ => *slot = Some(waker.clone()),
            }
        }
    }
}

/// Thread-pool callback for a completed operation's event.
unsafe extern "system" fn completed(context: *mut c_void, _timed_out: bool) {
    // SAFETY: `context` is the boxed `Signal` of a live `Operation`, which
    // unregisters this wait (waiting for a running callback to return)
    // before that box is released.
    let signal = unsafe { &*context.cast::<Signal>() };
    let waker = signal.waker.lock().ok().and_then(|mut slot| slot.take());
    if let Some(waker) = waker {
        waker.wake();
    }
}

/// One overlapped read or write. Its `OVERLAPPED`, buffer, event and
/// `Signal` live on the heap or in the kernel, so moving the value never
/// moves what the kernel or the callback points at.
struct Operation {
    handle: HANDLE,
    overlapped: Box<OVERLAPPED>,
    buffer: Vec<u8>,
    /// Manual-reset completion event; stays set once the operation ends.
    event: OwnedHandle,
    signal: Box<Signal>,
    wait: Option<HANDLE>,
    /// The kernel has finished with the buffer and `OVERLAPPED`.
    done: bool,
}

// SAFETY: `HANDLE` and `OVERLAPPED` are not `Send` only because they hold
// raw pointers. These are kernel handles, valid from any thread of the
// process, and the `OVERLAPPED` is reached only through this value (and by
// the kernel). Every call made on them (`GetOverlappedResult`, `CancelIoEx`,
// which unlike `CancelIo` cancels I/O issued by any thread, and
// `UnregisterWaitEx`) may come from any thread. The callback shares only
// the `Signal`, behind its mutex.
unsafe impl Send for Operation {}

impl Operation {
    fn start(
        handle: HANDLE,
        buffer: Vec<u8>,
        direction: Direction,
        cx: &Context<'_>,
    ) -> io::Result<Self> {
        // SAFETY: an unnamed manual-reset event with default security.
        let event = unsafe { CreateEventW(None, true, false, None) }.map_err(|e| os(&e))?;
        // SAFETY: the new event handle, owned only by this value from here.
        let event = unsafe { OwnedHandle::from_raw_handle(event.0) };
        let mut operation = Self {
            handle,
            overlapped: Box::new(OVERLAPPED {
                hEvent: HANDLE(event.as_raw_handle()),
                ..Default::default()
            }),
            buffer,
            event,
            signal: Box::new(Signal {
                waker: Mutex::new(Some(cx.waker().clone())),
            }),
            wait: None,
            done: true,
        };
        let overlapped: *mut OVERLAPPED = &raw mut *operation.overlapped;
        // SAFETY: the buffer and `OVERLAPPED` are heap allocations owned by
        // `operation`, whose `Drop` cancels and drains an unfinished
        // operation before releasing them.
        let started = unsafe {
            match direction {
                Direction::Read => ReadFile(
                    handle,
                    Some(operation.buffer.as_mut_slice()),
                    None,
                    Some(overlapped),
                ),
                Direction::Write => WriteFile(
                    handle,
                    Some(operation.buffer.as_slice()),
                    None,
                    Some(overlapped),
                ),
            }
        };
        if let Err(error) = started
            && !is_win32(&error, ERROR_IO_PENDING)
        {
            // Nothing was queued; the kernel holds no reference.
            return Err(os(&error));
        }
        operation.done = false;
        let mut wait = HANDLE::default();
        let context: *const Signal = &raw const *operation.signal;
        // SAFETY: one wait on this operation's event, whose context is the
        // boxed `Signal`; `Drop` unregisters it before the box is released.
        // On failure `operation` drops here, cancelling the started I/O.
        unsafe {
            RegisterWaitForSingleObject(
                &raw mut wait,
                HANDLE(operation.event.as_raw_handle()),
                Some(completed),
                Some(context.cast()),
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        }
        .map_err(|e| os(&e))?;
        operation.wait = Some(wait);
        Ok(operation)
    }

    /// Bytes moved, once the operation has ended.
    fn poll(&mut self, cx: &Context<'_>) -> Poll<io::Result<usize>> {
        // The waker is current before the completion is checked, so a
        // completion after the check wakes this task.
        self.signal.set(cx.waker());
        let mut moved = 0_u32;
        // SAFETY: asks without waiting about the operation started with this
        // live `OVERLAPPED` on the still-open handle.
        let result = unsafe {
            GetOverlappedResult(
                self.handle,
                &raw const *self.overlapped,
                &raw mut moved,
                false,
            )
        };
        match result {
            Err(error) if is_win32(&error, ERROR_IO_INCOMPLETE) => Poll::Pending,
            // A message-mode pipe's partial read still carries data.
            Err(error) if is_win32(&error, ERROR_MORE_DATA) => {
                self.done = true;
                Poll::Ready(Ok(moved as usize))
            }
            Err(error) => {
                self.done = true;
                Poll::Ready(Err(os(&error)))
            }
            Ok(()) => {
                self.done = true;
                Poll::Ready(Ok(moved as usize))
            }
        }
    }

    fn into_buffer(mut self) -> Vec<u8> {
        core::mem::take(&mut self.buffer)
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        if !self.done {
            let mut moved = 0_u32;
            // SAFETY: cancels exactly this operation and waits for it to
            // end, after which the kernel holds no reference to the buffer
            // or `OVERLAPPED`. The handle is still open (`PipeClient` drops
            // its operations first).
            unsafe {
                let _ = CancelIoEx(self.handle, Some(&raw const *self.overlapped));
                let _ = GetOverlappedResult(
                    self.handle,
                    &raw const *self.overlapped,
                    &raw mut moved,
                    true,
                );
            }
        }
        if let Some(wait) = self.wait.take() {
            // SAFETY: `INVALID_HANDLE_VALUE` makes this wait for a running
            // callback to return; none starts afterwards, so the `Signal`
            // may then be released.
            unsafe {
                let _ = UnregisterWaitEx(wait, Some(INVALID_HANDLE_VALUE));
            }
        }
    }
}

#[cfg(test)]
mod tests;
