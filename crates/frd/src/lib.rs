#![forbid(unsafe_code)]
// Composed native futures need a deeper compiler proof of their Send bounds.
// This changes no runtime stack, queue, protocol, or recursion allowance.
#![recursion_limit = "256"]
//! Host broker integration. Native capture/codec code runs in child processes,
//! never in an authority task. Asupersync QUIC remains the primary transport.
#[cfg(target_os = "linux")]
pub mod worker;

#[cfg(target_os = "linux")]
pub mod media;

#[cfg(target_os = "linux")]
pub mod linux;

pub mod input_watchdog;

pub mod input_agent;

pub mod session_agent;

pub mod status;

pub mod service_install;

#[cfg(target_os = "linux")]
pub mod media_egress;

#[cfg(target_os = "linux")]
pub mod media_quic;

#[cfg(target_os = "linux")]
pub mod input_quic;

#[cfg(target_os = "linux")]
pub mod session_startup;

#[cfg(target_os = "linux")]
pub mod display_selection;

#[cfg(target_os = "linux")]
pub mod native_connection;

#[cfg(target_os = "linux")]
pub mod clipboard_quic;

#[cfg(target_os = "linux")]
pub mod native_clipboard;

#[cfg(target_os = "linux")]
pub mod local_sharing;

#[cfg(target_os = "linux")]
pub mod broker;

#[cfg(target_os = "linux")]
pub mod host_policy;

// `frd run`: the installed host service composed from the modules above.
#[cfg(target_os = "linux")]
pub mod host_run;
