//! Linux parent-task death binding, installed before any native codec call
//! or X11 connection in a private child (media worker, input executor).
use core::ffi::{c_int, c_ulong};
unsafe extern "C" {
    fn getppid() -> c_int;
    fn prctl(option: c_int, ...) -> c_int;
}
/// The child was already reparented, or the kernel refused the binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParentUnbound;
/// Request kernel termination when the spawning Linux task dies. Both parent
/// checks refuse already-reparented workers. No privileges or signal handler
/// are installed. The owner must also supervise ordinary timeout/cancel paths.
///
/// Linux binds this to the spawning THREAD, not all threads of its process;
/// launch from a runtime task whose underlying thread remains alive. Do not
/// change credentials or exec another privileged image after installing it.
/// See Linux `PR_SET_PDEATHSIG(2const)`, including its credential/thread caveats.
pub fn bind_parent(expected_parent: u32) -> Result<(), ParentUnbound> {
    let expected = c_int::try_from(expected_parent).map_err(|_| ParentUnbound)?;
    if expected <= 1 {
        return Err(ParentUnbound);
    }
    // SAFETY: scalar libc calls; all variadic prctl arguments have machine-word
    // width. PR_SET_PDEATHSIG=1, SIGKILL=9 are Linux UAPI constants. No Rust
    // memory, callbacks or signal handlers are exposed to the kernel.
    unsafe {
        if getppid() != expected
            || prctl(1, 9 as c_ulong, 0 as c_ulong, 0 as c_ulong, 0 as c_ulong) != 0
            || getppid() != expected
        {
            return Err(ParentUnbound);
        }
    }
    Ok(())
}
/// The media worker's original typed form of `bind_parent`.
#[cfg(feature = "linux-media")]
pub fn bind_worker_parent(expected_parent: u32) -> Result<(), crate::NativeError> {
    bind_parent(expected_parent).map_err(|_| crate::NativeError::Unavailable)
}
