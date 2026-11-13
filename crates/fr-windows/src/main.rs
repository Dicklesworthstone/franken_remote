//! The `FrankenRemote` Windows client.
//!
//! Windows has no readable Tailscale `LocalAPI` here yet, so the client names
//! a host by its exact `MagicDNS` name (`<machine>.<tailnet>.ts.net`) the way
//! the iOS app does: the name must resolve inside the tailnet ranges and the
//! host must present a Let's Encrypt certificate for exactly that name. The
//! host's own `WhoIs` admission and policy decide what this machine may do.
//!
//! Commands:
//!   fr-windows view <name> [--port N] [--address IP]... [--control] [--seconds N]
//!       one session in a native window: Media Foundation (DXVA) decode and
//!       a D3D11 swap chain, 1:1 when the picture fits. Prints a JSON
//!       completion document (counts and timings, no content) at the end.
//!   fr-windows directory <name> [--port N] [--address IP]...
//!       the host's tailnet directory (JSON).
//!   fr-windows capture-probe [--seconds N] [--fps N] [--bitrate N] [--display N] [--all]
//!       this machine as a host: Desktop Duplication into the GPU's HEVC
//!       encoder, admitted and then decoded here. Prints counts and timings
//!       (JSON). Run it in the interactive session.
//!
//! `--address` dials those tailnet addresses instead of resolving the name
//! (Windows `MagicDNS` answers only A records); TLS still verifies exactly
//! `<name>`.
#[cfg(windows)]
mod windows;

// Platform-neutral pieces of the viewer, tested on every CI host.
#[cfg_attr(not(windows), allow(dead_code))]
mod geometry;
#[cfg_attr(not(windows), allow(dead_code))]
mod keys;

// The original capture policy is safe and platform-neutral; exercise it here
// without compiling the platform session and media bindings.
#[cfg(test)]
#[path = "../../fr-mobile-ffi/src/session/key_capture.rs"]
mod key_capture_policy;

use std::process::ExitCode;

const USAGE: &str = "usage: fr-windows view <machine>.<tailnet>.ts.net [--port N] [--address IP]... \
     [--control] [--seconds N]\n       fr-windows directory <machine>.<tailnet>.ts.net [--port N] \
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
