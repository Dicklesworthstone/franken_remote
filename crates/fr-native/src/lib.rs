//! Native media adapters. `FFmpeg` and X11 stay behind this named FFI boundary.
#![deny(unsafe_op_in_unsafe_fn)]

pub mod macos;
pub mod macos_presentation;
pub mod wayland;
pub mod windows;

#[cfg(all(target_os = "linux", feature = "linux-opus"))]
pub mod opus;

#[cfg(all(target_os = "linux", feature = "linux-pulse-playback"))]
pub mod pulse;

#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub mod cursor;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
mod damage;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
mod linux;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub use linux::*;

#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub mod capture;

#[cfg(all(target_os = "linux", feature = "linux-media"))]
mod parent;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub use parent::bind_worker_parent;

#[cfg(all(target_os = "linux", feature = "linux-input"))]
pub mod input;
#[cfg(all(target_os = "linux", feature = "linux-input"))]
mod keyboard;

#[cfg(all(target_os = "linux", feature = "linux-input-agent"))]
pub mod input_agent;
#[cfg(all(
    target_os = "linux",
    any(feature = "linux-media", feature = "linux-input")
))]
pub mod xlib;

#[cfg(all(target_os = "linux", feature = "linux-displays"))]
pub mod displays;

#[cfg(all(target_os = "linux", feature = "linux-clipboard"))]
pub mod clipboard;

#[cfg(all(target_os = "linux", feature = "linux-clipboard"))]
pub mod clipboard_observation;

#[cfg(all(target_os = "linux", feature = "linux-session-ui"))]
pub mod sharing_indicator;

#[cfg(all(
    target_os = "linux",
    any(feature = "linux-input", feature = "linux-viewer-input")
))]
mod key_names;

#[cfg(all(target_os = "linux", feature = "linux-viewer-input"))]
pub mod viewer_input;

#[cfg(all(target_os = "linux", feature = "linux-viewer-window"))]
pub mod viewer_window;

#[cfg(all(target_os = "linux", feature = "linux-desktop"))]
pub mod desktop;

#[cfg(all(target_os = "linux", feature = "linux-viewer-window"))]
pub mod display_picker;

pub mod mobile_ffi;
