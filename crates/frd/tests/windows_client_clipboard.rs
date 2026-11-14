//! Exercise the native client's real clipboard ownership and offer policies
//! without loading its platform decoder or opening an OS clipboard.
#![cfg(target_os = "linux")]

#[allow(dead_code)]
#[path = "../../fr-mobile-ffi/src/session/clipboard.rs"]
mod clipboard;

// These are the four content-free result variants used by control_policy.
// The complete native session and platform input adapters stay out of this
// portable policy test; the policy implementation itself is included verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputResult {
    Sent,
    Unsupported,
    Busy,
    ControlLost,
}

#[path = "../../fr-mobile-ffi/src/session/control_policy.rs"]
mod control_policy;
