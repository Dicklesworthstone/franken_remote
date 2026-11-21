//! The `FrankenRemote` Windows client: one library behind two executables.
//!
//! * `FrankenRemote.exe` (`app`) is the desktop application: a windowed
//!   machine picker that opens a viewer, returns to the picker when the
//!   session ends, and can install itself for the current user (Start menu
//!   entry, Apps & features uninstall). It has no console.
//! * `fr-windows.exe` (`cli`) is the command line described below.
//!
//! The installed Tailscale service supplies bounded machine discovery and
//! authenticated node identity. A selected stable ID and certificate name are
//! revalidated before dialing; strict TLS and the host's own admission policy
//! still decide whether a session may start.
//!
//! Commands:
//!   fr-windows app [--port N]
//!       open the packaged desktop app's machine picker on this service port.
//!   fr-windows
//!       choose a machine from the installed Tailscale snapshot, then view it.
//!   fr-windows hosts
//!       a local machine-choice snapshot (JSON), without remote probes.
//!   fr-windows view [stable-id-or-name] [--port N] [--ipv4|--ipv6]
//!       [--address IP]... [--control] [--no-clipboard] [--seconds N]
//!       [--fullscreen] [--no-shortcuts]
//!       [--display primary|only|choose|HANDLE] [--decoder auto|hardware|software]
//!       [--send FILE]... (requires --control; up to eight regular files, sent once)
//!       [--receive DIR] [--receive-max-file-bytes N] [--receive-max-session-bytes N]
//!       [--receive-conflict reject|keep-both] (requires --control and local consent)
//!       one session in a native window: Media Foundation decode (DXVA on the
//!       GPU when available, otherwise the decoder's CPU path uploaded to the
//!       GPU) and a D3D11 swap chain, 1:1 when the picture fits. Prints a JSON
//!       completion document (counts and timings, no content) at the end.
//!   fr-windows directory <stable-id-or-name> [--port N] [--ipv4|--ipv6]
//!       [--address IP]...
//!       the host's tailnet directory (JSON).
//!   fr-windows capture-probe [--seconds N] [--fps N] [--bitrate N] [--display N]
//!       [--picture-width N] [--all]
//!       this machine as a host: Desktop Duplication into the GPU's HEVC
//!       encoder (optionally a scaled picture N pixels wide), admitted and
//!       then decoded here. Prints counts and timings (JSON). Run it in the
//!       interactive session.
//!
//! `--address` constrains the complete authenticated address set; it never
//! replaces daemon metadata. Use `--ipv4` or `--ipv6` to select one family.
//! See docs/windows-client.md for selection, failures and validation scope.
#[cfg(windows)]
mod windows;

// Platform-neutral pieces of the viewer, tested on every CI host.
#[cfg_attr(not(windows), allow(dead_code))]
mod change_display;
#[cfg_attr(not(windows), allow(dead_code))]
mod control;
#[cfg_attr(not(windows), allow(dead_code))]
mod cursor;
#[cfg_attr(not(windows), allow(dead_code))]
mod cursor_image;
#[cfg_attr(not(windows), allow(dead_code))]
mod file_choice;
#[cfg_attr(not(windows), allow(dead_code))]
mod geometry;
#[cfg(any(windows, test))]
mod host_picker;
#[cfg_attr(not(windows), allow(dead_code))]
mod install_plan;
#[cfg_attr(not(windows), allow(dead_code))]
mod keys;
// Picker rows read the shared catalog's path hints (fr-client is a
// Windows dependency and a dev-dependency everywhere else).
#[cfg(any(windows, test))]
#[cfg_attr(not(windows), allow(dead_code))]
mod machines;
#[cfg_attr(not(windows), allow(dead_code))]
mod options;
#[cfg_attr(not(windows), allow(dead_code))]
mod outcome;
#[cfg_attr(not(windows), allow(dead_code))]
mod receive_choice;
// Shared completion capture uses the same Windows/dev-only core as the picker.
#[cfg(any(windows, test))]
#[cfg_attr(not(windows), allow(dead_code))]
mod session_output;
#[cfg_attr(not(windows), allow(dead_code))]
mod settings;
#[cfg_attr(not(windows), allow(dead_code))]
mod text;

// The original capture policy is safe and platform-neutral; exercise it here
// without compiling the platform session and media bindings.
#[cfg(test)]
#[path = "../../fr-mobile-ffi/src/session/key_capture.rs"]
mod key_capture_policy;

#[cfg(test)]
#[path = "../../fr-mobile-ffi/src/input/scroll.rs"]
mod scroll_capture_policy;

// Exercise the same original-catalog selection used by the native session
// without compiling its platform decoder or async runtime on portable hosts.
#[cfg(test)]
#[path = "../../fr-mobile-ffi/src/session/display.rs"]
mod display_selection_policy;

#[cfg(test)]
#[allow(dead_code)]
#[path = "../../fr-mobile-ffi/src/session/control_request.rs"]
mod control_request_policy;

#[cfg(test)]
#[path = "../../fr-mobile-ffi/src/session/focus_close.rs"]
mod focus_close_policy;

// Check the actual shared upload batch accounting without substituting its
// retained-handle selection or compiling the platform session and decoder.
#[cfg(test)]
#[path = "../../fr-mobile-ffi/src/session/files/selection_budget.rs"]
mod upload_selection_budget;

use std::process::ExitCode;

const USAGE: &str = "usage: fr-windows\n       fr-windows app [--port N]\n       fr-windows hosts\n       fr-windows view [stable-id-or-full-name] \
     [--port N] [--ipv4|--ipv6] [--address IP]... [--control] [--no-clipboard] [--seconds N] \
     [--fullscreen] [--no-shortcuts] [--display primary|only|choose|HANDLE] [--decoder auto|hardware|software] [--send FILE]... [--receive DIR] \
     [--receive-max-file-bytes N] [--receive-max-session-bytes N] \
     [--receive-conflict reject|keep-both]\n       \
     fr-windows directory <stable-id-or-full-name> [--port N] [--ipv4|--ipv6] \
     [--address IP]...\n       fr-windows capture-probe [--seconds N] [--fps N] [--bitrate N] \
     [--display N] [--picture-width N] [--all]\n";

/// The `fr-windows` command line (arguments without the program name).
pub fn cli(args: &[String]) -> ExitCode {
    #[cfg(windows)]
    return windows::run(args);
    #[cfg(not(windows))]
    {
        let _ = args;
        eprint!("fr-windows runs on Windows only.\n{USAGE}");
        ExitCode::from(2)
    }
}

/// The `FrankenRemote` desktop application. It reads its own arguments:
/// `--port N` (the picker, default 8443), `--install`, or `--uninstall`
/// (Apps & features). An explicit port belongs only to this launch.
pub fn app() -> ExitCode {
    #[cfg(windows)]
    return windows::app::run();
    #[cfg(not(windows))]
    {
        eprintln!("FrankenRemote's desktop application runs on Windows only.");
        ExitCode::from(2)
    }
}
