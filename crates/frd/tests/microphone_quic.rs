#![cfg(target_os = "linux")]
//! Compile the exact production host coordinator with its original private
//! tests, without linking the daemon's unrelated unit-test executable. The
//! authority, input agent and process supervisor are the actual frd library.
pub use frd::{input_agent, input_quic, input_watchdog, microphone_process};

#[path = "../src/microphone_quic.rs"]
pub mod microphone_quic;
