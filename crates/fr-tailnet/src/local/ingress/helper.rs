//! Least-privilege split of Linux ingress enforcement (plan sections 5.2, 19.2).
//!
//! `frd ingress-helper` runs as root and owns the project's drop rule for an
//! unprivileged broker. Its ENTIRE API is `install`, `renew` and `remove` of one
//! rule per connection: (configured tailscale interface, an address currently
//! assigned to it, a nonzero port, a protocol set within {udp, tcp}). No caller
//! text reaches nft; the script, table name and interface index are the helper's.
//!
//! Every connection is admitted by `SO_PEERCRED` against uids in a ROOT-OWNED
//! configuration. A rule lives exactly as long as its connection: close or crash
//! of the broker removes it; stale `frdh_` tables of a dead helper are reclaimed
//! at the next start. Generations fence renew/remove. This is crash isolation and
//! a least-privilege split, not a sandbox: root, the kernel, nft/ip and the
//! selected TUN remain trusted, and the unprivileged broker trusts the helper's
//! read-back because it cannot read the ruleset itself.
use super::{Protocols, interface_valid};
use std::path::Path;

mod codec;
pub use codec::{
    Install, MAX_REQUEST, MAX_RESPONSE, Refusal, Request, Response, VERSION, decode_request,
    decode_response, encode_request, encode_response,
};
mod client;
pub(super) use client::Link;
mod server;
pub use server::{Event, ServeError, Settings, SettingsError, serve};

/// Root-owned runtime directory (systemd `RuntimeDirectory=`), not user-writable.
pub const DEFAULT_SOCKET: &str = "/run/frankenremote-ingress/helper.sock";
/// Root-owned configuration naming the interface and admitted uids.
pub const DEFAULT_CONFIG: &str = "/etc/frankenremote/ingress-helper.json";
/// The helper creates, removes and reclaims only these tables; the direct
/// path's `frd_` tables are never touched by it.
pub const TABLE_PREFIX: &str = "frdh_";

/// Only a helper-generated table: the prefix plus exactly 32 lowercase hex.
pub fn owned_table(name: &str) -> Result<(), Refusal> {
    let hex = name
        .strip_prefix(TABLE_PREFIX)
        .ok_or(Refusal::ForeignTable)?;
    if hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Ok(())
    } else {
        Err(Refusal::ForeignTable)
    }
}
/// Absolute, bounded for `sun_path`, no NUL or parent components.
pub(super) fn socket_path_valid(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    path.is_absolute()
        && bytes.len() < 108
        && !bytes.contains(&0)
        && !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

#[cfg(test)]
mod tests;
