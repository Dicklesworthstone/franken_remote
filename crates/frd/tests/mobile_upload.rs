//! Run the actual native upload owner against the shared transfer implementation.
//! No platform decoder or native window is loaded by these policy regressions.
#![cfg(target_os = "linux")]

#[path = "../../fr-mobile-ffi/src/session/files.rs"]
#[allow(dead_code)]
mod files;
