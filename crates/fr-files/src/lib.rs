#![forbid(unsafe_code)]
//! Transactional materialization for the desktop session's ATP file lane.
//!
//! This crate does not listen, admit peers, or grant file access. The Linux
//! storage boundary writes regular files and bounded portable file trees into a
//! locally selected directory; it never imports links or filesystem metadata.
//! All filesystem operations belong on an owned disk worker, not a reactor or
//! an authority task. Network attachment and permission remain with the session.

#[cfg(target_os = "linux")]
pub mod receive;

#[cfg(target_os = "linux")]
pub mod session;

#[cfg(target_os = "linux")]
pub mod worker;

#[cfg(target_os = "linux")]
pub mod atp;

#[cfg(target_os = "linux")]
pub mod wire;

#[cfg(target_os = "linux")]
pub mod quic;

/// Bounded native file sender using locally selected descriptors.
#[cfg(target_os = "linux")]
pub mod sender;
