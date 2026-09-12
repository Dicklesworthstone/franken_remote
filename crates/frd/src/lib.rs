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

pub mod input_watchdog;

pub mod input_agent;

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
