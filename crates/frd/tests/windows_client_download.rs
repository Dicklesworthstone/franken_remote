//! Exercise the native receiver's actual destination/owner/report policy on
//! Linux without loading the Windows renderer. This is not Windows DACL proof.
#![cfg(target_os = "linux")]

#[allow(dead_code)]
#[path = "../../fr-mobile-ffi/src/session/download.rs"]
mod download;
