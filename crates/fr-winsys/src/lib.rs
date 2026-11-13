//! Win32 calls for the workspace's safe-Rust crates (fr-tailnet first, then
//! frd's Windows host), behind safe APIs. Platform adapters that already
//! hold FFI (fr-native, fr-windows) keep their own bindings.
//!
//! Every unsafe block states the invariant that makes it sound.
#[cfg(windows)]
pub mod certs;
#[cfg(windows)]
pub mod fs;
#[cfg(windows)]
pub mod net;
#[cfg(windows)]
pub mod pipe;
#[cfg(windows)]
pub mod session;
#[cfg(windows)]
pub mod time;
