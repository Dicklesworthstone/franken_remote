//! Native media adapters. `FFmpeg` and X11 stay behind this named FFI boundary.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(all(target_os = "linux", feature = "linux-media"))]
mod linux;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub use linux::*;

#[cfg(all(target_os = "linux", feature = "linux-media"))]
mod parent;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub use parent::bind_worker_parent;

#[cfg(all(target_os = "linux", feature = "linux-input"))]
pub mod input;
