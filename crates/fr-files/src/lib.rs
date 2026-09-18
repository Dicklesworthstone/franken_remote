#![forbid(unsafe_code)]
//! Transactional materialization for the desktop session's ATP file lane.
//!
//! This crate does not listen, admit peers, or grant file access. The Linux
//! storage boundary writes regular files only, into a locally selected directory.
//! All filesystem operations belong on an owned disk worker, not a reactor or
//! an authority task. Network attachment and permission remain with the session.

#[cfg(target_os = "linux")]
pub mod receive;

#[cfg(target_os = "linux")]
pub mod session;

#[cfg(target_os = "linux")]
pub mod worker;
