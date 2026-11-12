//! The `FrankenRemote` iOS client's Rust core, exported to the Swift app
//! through the hand-written C ABI in `include/fr_mobile.h`.
//!
//! The app names a host by its `MagicDNS` name (`<machine>.<tailnet>.ts.net`);
//! the session resolves it, refuses any address outside the tailnet ranges,
//! verifies the host's Let's Encrypt certificate for exactly that name against
//! the bundled ISRG roots, and pins the addresses it saw so a later connection
//! that resolves elsewhere is refused (see docs/ios-client.md). Tailscale's
//! network extension carries the packets and is the authority for which
//! machines this device may reach; frd's host policy decides whether the
//! viewer may watch or control.
//!
//! iOS links it for the app and macOS runs the host tests of the same code;
//! Windows builds the session core for its client (`windows-session`, no C
//! ABI yet). Other platforms compile an empty crate.
#![cfg_attr(
    not(any(target_os = "ios", target_os = "macos", windows)),
    allow(unused)
)]

#[cfg(any(target_os = "ios", target_os = "macos", windows))]
pub mod events;
#[cfg(any(target_os = "ios", target_os = "macos", windows))]
pub mod input;
#[cfg(any(target_os = "ios", target_os = "macos", windows))]
pub mod roots;
#[cfg(any(target_os = "ios", target_os = "macos", windows))]
pub mod video;

#[cfg(all(
    feature = "session",
    any(
        target_os = "ios",
        all(target_os = "macos", feature = "macos-session"),
        all(windows, feature = "windows-session")
    )
))]
pub mod session;

#[cfg(all(
    feature = "session",
    any(
        target_os = "ios",
        all(target_os = "macos", feature = "macos-session"),
        all(windows, feature = "windows-session")
    )
))]
pub mod directory;

#[cfg(all(
    feature = "session",
    any(target_os = "ios", all(target_os = "macos", feature = "macos-session"))
))]
pub mod ffi;
