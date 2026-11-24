//! Win32 calls for the workspace's safe-Rust crates (fr-tailnet first, then
//! frd's Windows host), behind safe APIs. Platform adapters that already
//! hold FFI (fr-native, fr-windows) keep their own bindings.
//!
//! Every unsafe block states the invariant that makes it sound.
#[cfg(all(windows, feature = "full"))]
pub mod approval;
#[cfg(any(test, all(windows, feature = "full")))]
mod approval_policy;
#[cfg(all(windows, feature = "full"))]
pub mod certs;
#[cfg(all(windows, feature = "full"))]
pub mod clipboard;
#[cfg(any(test, all(windows, feature = "protected-files")))]
mod file_selection_policy;
#[cfg(all(windows, feature = "protected-files"))]
pub mod fs;
#[cfg(all(windows, feature = "full"))]
pub mod indicator;
#[cfg(all(windows, feature = "full"))]
pub mod input;
#[cfg(all(windows, feature = "full"))]
pub mod net;
#[cfg(all(windows, feature = "protected-files"))]
pub mod pipe;
#[cfg(all(windows, feature = "full"))]
pub mod session;
#[cfg(all(windows, feature = "full"))]
pub mod time;
