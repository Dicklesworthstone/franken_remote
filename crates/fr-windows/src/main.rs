//! The `FrankenRemote` Windows client.
//!
//! The installed Tailscale service supplies bounded machine discovery and
//! authenticated node identity. A selected stable ID and certificate name are
//! revalidated before dialing; strict TLS and the host's own admission policy
//! still decide whether a session may start.
//!
//! Commands:
//!   fr-windows hosts
//!       a local machine-choice snapshot (JSON), without remote probes.
//!   fr-windows view <stable-id-or-name> [--port N] [--ipv4|--ipv6]
//!       [--address IP]... [--control] [--seconds N]
//!       one session in a native window: Media Foundation (DXVA) decode and
//!       a D3D11 swap chain, 1:1 when the picture fits. Prints a JSON
//!       completion document (counts and timings, no content) at the end.
//!   fr-windows directory <stable-id-or-name> [--port N] [--ipv4|--ipv6]
//!       [--address IP]...
//!       the host's tailnet directory (JSON).
//!   fr-windows capture-probe [--seconds N] [--fps N] [--bitrate N] [--display N] [--all]
//!       this machine as a host: Desktop Duplication into the GPU's HEVC
//!       encoder, admitted and then decoded here. Prints counts and timings
//!       (JSON). Run it in the interactive session.
//!
//! `--address` constrains the complete authenticated address set; it never
//! replaces daemon metadata. Use `--ipv4` or `--ipv6` to select one family.
//! See docs/windows-client.md for selection, failures and validation scope.
#[cfg(windows)]
mod windows;

// Platform-neutral pieces of the viewer, tested on every CI host.
#[cfg_attr(not(windows), allow(dead_code))]
mod geometry;
#[cfg_attr(not(windows), allow(dead_code))]
mod keys;
#[cfg_attr(not(windows), allow(dead_code))]
mod options;

// The original capture policy is safe and platform-neutral; exercise it here
// without compiling the platform session and media bindings.
#[cfg(test)]
#[path = "../../fr-mobile-ffi/src/session/key_capture.rs"]
mod key_capture_policy;

#[cfg(test)]
#[path = "../../fr-mobile-ffi/src/input/scroll.rs"]
mod scroll_capture_policy;

use std::process::ExitCode;

const USAGE: &str = "usage: fr-windows hosts\n       fr-windows view <stable-id-or-full-name> \
     [--port N] [--ipv4|--ipv6] [--address IP]... [--control] [--seconds N]\n       \
     fr-windows directory <stable-id-or-full-name> [--port N] [--ipv4|--ipv6] \
     [--address IP]...\n       fr-windows capture-probe [--seconds N] [--fps N] [--bitrate N] \
     [--display N] [--all]\n";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    return windows::run(&args);
    #[cfg(not(windows))]
    {
        let _ = args;
        eprint!("fr-windows runs on Windows only.\n{USAGE}");
        ExitCode::from(2)
    }
}
