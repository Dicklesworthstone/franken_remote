//! Kernel restrictions for the dedicated CPU decoding child, not its parent.
use super::X11Surface;
use core::ffi::{c_int, c_void};
use std::os::fd::{AsRawFd, BorrowedFd};
unsafe extern "C" {
    fn fr_x11_confine_decoder(
        surface: *mut c_void,
        input: c_int,
        output: c_int,
        diagnostic: c_int,
    ) -> c_int;
}
/// No native error strings, credentials or media content in diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecoderSandboxUnavailable;
impl X11Surface {
    /// Irreversibly restrict the process and all existing threads before any
    /// host-supplied configuration or picture reaches `FFmpeg`. Call only in the
    /// dedicated software decoder, after opening its selected X11 surface and
    /// original parent pipes; this is not an automatic restriction on library users.
    ///
    /// The qualified Linux x86-64 profile admits descriptor I/O only through
    /// these pipes and this surface's original X connection. New filesystem
    /// access, connections, execution, process/thread creation, foreign-process
    /// access, general ioctls, file mappings and new executable memory are denied.
    /// The X server still has broad desktop and descriptor-passing authority:
    /// this is partial containment, NOT X11 input isolation or a complete sandbox.
    /// Failure is terminal; never retry decoder startup unrestricted. Other
    /// architectures refuse until separately qualified. This operation is one-way.
    pub fn confine_decoder_process(
        &mut self,
        input: BorrowedFd<'_>,
        output: BorrowedFd<'_>,
        diagnostic: BorrowedFd<'_>,
    ) -> Result<(), DecoderSandboxUnavailable> {
        // SAFETY: native owner and all borrowed descriptors live through the
        // call. The kernel copies fixed BPF storage; no pointer/callback escapes,
        // and no descriptor ownership is transferred or closed here.
        let installed = unsafe {
            fr_x11_confine_decoder(
                self.raw.as_ptr(),
                input.as_raw_fd(),
                output.as_raw_fd(),
                diagnostic.as_raw_fd(),
            )
        };
        if installed == 1 {
            Ok(())
        } else {
            Err(DecoderSandboxUnavailable)
        }
    }
}
