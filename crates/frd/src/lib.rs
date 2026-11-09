#![forbid(unsafe_code)]
// The macOS viewer build compiles frd's shared session modules, whose host
// half (capture, injection, ingress, approval) is reached only from the
// Linux-only host run until the macOS host lands (fr-p1-host-macos-vif).
// The macos-host build compiles that run while it is being wired: Linux-only
// paths it does not reach yet (Xvfb provisioning, X11 indicators) stay quiet.
// iOS compiles the same shared modules for its viewer (fr-mobile-ffi) and
// never reaches their host half.
#![cfg_attr(
    any(
        target_os = "ios",
        all(
            target_os = "macos",
            any(feature = "macos-viewer", feature = "macos-host")
        )
    ),
    allow(dead_code, unused_imports, unused_variables)
)]
// Composed native futures need a deeper compiler proof of their Send bounds.
// This changes no runtime stack, queue, protocol, or recursion allowance.
#![recursion_limit = "256"]
//! Host broker integration. Native capture/codec code runs in child processes,
//! never in an authority task. Asupersync QUIC remains the primary transport.
#[cfg(all(test, target_os = "linux"))]
mod test_support;

#[cfg(native_stack)]
pub mod worker;

#[cfg(native_stack)]
pub mod media;

#[cfg(target_os = "linux")]
pub mod linux;

pub mod input_watchdog;

pub mod input_agent;

#[cfg(native_stack)]
pub mod input_process;

// Original controller-bound virtual microphone with two supervised native children.
#[cfg(native_stack)]
pub mod microphone_process;

#[cfg(native_stack)]
pub mod microphone_quic;

pub mod session_agent;

pub mod status;

pub mod service_install;

#[cfg(native_stack)]
pub mod media_egress;

#[cfg(native_stack)]
pub mod media_quic;

#[cfg(native_stack)]
pub mod input_quic;

#[cfg(native_stack)]
pub mod session_startup;

#[cfg(native_stack)]
pub mod display_selection;

#[cfg(native_stack)]
pub mod native_connection;

#[cfg(native_stack)]
pub mod clipboard_quic;

#[cfg(native_stack)]
pub mod native_clipboard;
#[cfg(native_stack)]
pub mod native_microphone;

// The controlled share's out-of-process X11 clipboard owner (never Xlib in frd).
#[cfg(native_stack)]
pub mod clipboard_process;

// Explicit viewer-to-host file sending into an operator-approved drop directory.
#[cfg(native_stack)]
pub mod native_files;

#[cfg(native_stack)]
pub mod local_sharing;

// Parent-owned terminal consent; no authority, capture or native input here.
#[cfg(host_stack)]
pub mod local_approval;

#[cfg(native_stack)]
pub mod broker;

#[cfg(host_stack)]
pub mod host_policy;

// `frd run`: the installed host service composed from the modules above.
#[cfg(host_stack)]
pub mod host_run;

// Opt-in read-only desktop indicator around the original host run lifetime.
#[cfg(native_stack)]
pub mod host_indicator;

#[cfg(host_stack)]
pub mod session_monitor;
// One persistent SIGINT/SIGTERM wait for `frd run` and the ingress helper.
#[cfg(host_stack)]
mod signals;

// `frd ingress-helper`: root owner of the ingress rule for an unprivileged run.
#[cfg(target_os = "linux")]
pub mod ingress_helper;

// Private launcher shared by read-only and one-use consent UI children.
#[cfg(native_stack)]
mod session_ui_process;

// Executable test fixture images, shared with the integration tests.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[path = "../tests/support/executable.rs"]
mod executable;
