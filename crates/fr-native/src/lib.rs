//! Native media adapters. `FFmpeg` and X11 stay behind this named FFI boundary.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(all(target_os = "linux", feature = "linux-opus"))]
pub mod opus;

#[cfg(all(target_os = "linux", feature = "linux-pulse-playback"))]
pub mod pulse;

#[cfg(all(target_os = "linux", feature = "linux-microphone-process"))]
pub mod audio_process;

#[cfg(all(target_os = "linux", feature = "linux-pipewire-mic"))]
pub mod pipewire_mic;

#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub mod cursor;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
mod damage;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
mod linux;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub use linux::*;

#[cfg(any(
    all(
        target_os = "linux",
        any(feature = "linux-media", feature = "linux-input-monitors")
    ),
    all(
        target_os = "macos",
        any(feature = "macos-desktop", feature = "macos-media")
    )
))]
mod error;
#[cfg(any(
    all(
        target_os = "linux",
        any(feature = "linux-media", feature = "linux-input-monitors")
    ),
    all(
        target_os = "macos",
        any(feature = "macos-desktop", feature = "macos-media")
    )
))]
pub use error::NativeError;

// macOS client presentation and input capture (fr-5a4w).
#[cfg(all(target_os = "macos", feature = "macos-desktop"))]
pub mod macos;
#[cfg(all(target_os = "macos", feature = "macos-media"))]
pub mod macos_capture;
#[cfg(all(target_os = "macos", feature = "macos-clipboard"))]
pub mod macos_clipboard;
#[cfg(any(test, all(target_os = "macos", feature = "macos-clipboard")))]
mod macos_clipboard_privacy;
#[cfg(any(test, all(target_os = "macos", feature = "macos-clipboard")))]
mod macos_clipboard_state;
// Pure keycode data: compiled for tests everywhere so ordinary CI checks it.
#[cfg(any(
    test,
    all(
        target_os = "macos",
        any(feature = "macos-desktop", feature = "macos-input")
    )
))]
pub mod macos_keys;
// Display geometry the capture catalog and the input executor share.
#[cfg(any(
    test,
    all(
        target_os = "macos",
        any(feature = "macos-media", feature = "macos-input")
    )
))]
pub mod macos_display;
// macOS host input injection for the per-lease executor (never frd).
#[cfg(all(target_os = "macos", feature = "macos-input"))]
pub mod macos_input;
#[cfg(any(test, all(target_os = "macos", feature = "macos-input")))]
pub mod macos_input_state;
#[cfg(any(test, all(target_os = "macos", feature = "macos-desktop")))]
mod macos_sizing;
// Invocation consent and native key gestures are portable policy, not AppKit.
#[cfg(any(test, all(target_os = "macos", feature = "macos-desktop")))]
mod macos_control;
#[cfg(any(test, all(target_os = "macos", feature = "macos-desktop")))]
mod macos_scroll;
#[cfg(any(test, all(target_os = "macos", feature = "macos-desktop")))]
mod macos_switch;
// File selection/one-use consent uses the real portable frd owners. Exercise
// it in Linux CI as well as in the native Mac desktop build.
#[cfg(all(test, target_os = "linux", feature = "linux-desktop"))]
#[path = "macos/files.rs"]
pub mod macos_files;

#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub mod capture;

#[cfg(all(
    target_os = "linux",
    any(feature = "linux-media", feature = "linux-desktop")
))]
pub mod host_picker;

#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub mod image_transfer;

#[cfg(all(
    target_os = "linux",
    any(
        feature = "linux-media",
        feature = "linux-input",
        feature = "linux-opus-process",
        feature = "linux-microphone-process"
    )
))]
mod parent;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub use parent::bind_worker_parent;
#[cfg(all(
    target_os = "linux",
    any(
        feature = "linux-media",
        feature = "linux-input",
        feature = "linux-opus-process",
        feature = "linux-microphone-process"
    )
))]
pub use parent::{ParentUnbound, bind_parent};

#[cfg(any(
    all(
        target_os = "linux",
        any(
            feature = "linux-input",
            feature = "linux-microphone-process",
            feature = "linux-microphone-playout"
        )
    ),
    all(target_os = "macos", feature = "macos-input")
))]
pub mod clock;

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

#[cfg(all(
    target_os = "linux",
    any(feature = "linux-displays", feature = "linux-input-monitors")
))]
pub mod displays;

#[cfg(all(target_os = "linux", feature = "linux-clipboard"))]
pub mod clipboard;

#[cfg(all(target_os = "linux", feature = "linux-clipboard"))]
pub mod clipboard_observation;

#[cfg(all(
    target_os = "linux",
    any(feature = "linux-session-ui", feature = "linux-input")
))]
pub mod sharing_indicator;

#[cfg(all(
    target_os = "linux",
    any(feature = "linux-input", feature = "linux-viewer-input")
))]
mod key_names;

#[cfg(all(target_os = "linux", feature = "linux-viewer-input"))]
pub mod viewer_input;

#[cfg(all(target_os = "linux", feature = "linux-viewer-input"))]
pub mod viewer_cursor;

#[cfg(all(target_os = "linux", feature = "linux-viewer-window"))]
pub mod viewer_window;

#[cfg(all(target_os = "linux", feature = "linux-desktop"))]
pub mod desktop;

#[cfg(all(target_os = "linux", feature = "linux-viewer-window"))]
pub mod display_picker;

#[cfg(all(target_os = "linux", feature = "linux-logind"))]
pub mod logind;

// Positive decisions remain confined to the supervised native session child.
#[cfg(all(target_os = "linux", feature = "linux-input"))]
pub mod approval_surface;

#[cfg(all(target_os = "linux", feature = "linux-desktop"))]
pub mod file_progress;

#[cfg(all(target_os = "macos", feature = "cli"))]
pub mod tailnet_macos;
