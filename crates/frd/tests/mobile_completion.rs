//! Exercise the native app's exact completion policy without loading its
//! platform decoder, window or certificate-export build script.
#![cfg(target_os = "linux")]

#[path = "../../fr-mobile-ffi/src/session/completion.rs"]
mod completion;
