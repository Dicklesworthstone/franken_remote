//! Explicit local aliases. Profiles never contain authority or addresses.
use super::{
    Failure,
    options::{SavedHostAction, SavedHostOptions, Target},
    output,
};
use fr_client::discovery::saved_peers::{SavedPeer, SavedPeerError, SavedPeers};
use std::{fmt::Write as _, path::PathBuf};
#[path = "saved_hosts/store.rs"]
mod store;
use store::{Change, Store};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Error {
    InvalidPath,
    UnsafePath,
    TooLarge,
    Busy,
    Io,
    Data(SavedPeerError),
}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}
impl From<rustix::io::Errno> for Error {
    fn from(e: rustix::io::Errno) -> Self {
        if e == rustix::io::Errno::LOOP {
            Self::UnsafePath
        } else {
            Self::Io
        }
    }
}
impl From<SavedPeerError> for Error {
    fn from(e: SavedPeerError) -> Self {
        Self::Data(e)
    }
}
fn failure(error: Error) -> Failure {
    let (code, next) = match error {
        Error::Data(SavedPeerError::Duplicate) => (
            "saved_host_duplicate",
            "The alias or peer endpoint is already saved. Inspect fr hosts saved; forget the old alias explicitly before replacing it.",
        ),
        Error::Data(SavedPeerError::NotFound) => (
            "saved_host_not_found",
            "List saved aliases with fr hosts saved; no live hostname or node-ID fallback is attempted.",
        ),
        Error::Data(SavedPeerError::Capacity) => (
            "saved_hosts_capacity",
            "At most 100 saved hosts are retained. Explicitly forget an unused alias before saving another.",
        ),
        Error::Busy => (
            "saved_hosts_busy",
            "Another local writer owns the saved-host store. Wait for it to finish before retrying.",
        ),
        Error::InvalidPath | Error::UnsafePath => (
            "saved_hosts_unsafe_path",
            "Use an absolute local --hosts-file path through trusted directories, with a private regular file owned by this user; symlinks and hard links are refused.",
        ),
        Error::TooLarge | Error::Data(SavedPeerError::TooLarge) => (
            "saved_hosts_too_large",
            "The saved-host document exceeds 128 KiB. Its existing contents were not replaced.",
        ),
        Error::Data(_) => (
            "saved_hosts_invalid",
            "The existing saved-host document has invalid, duplicate or unsupported fields. Inspect it locally; it was not replaced.",
        ),
        Error::Io => (
            "saved_hosts_io",
            "Check local saved-host directory permissions and available storage; no profile content is included in diagnostics.",
        ),
    };
    Failure::new(code, next, 1)
}
fn path(explicit: Option<&PathBuf>) -> Result<PathBuf, Error> {
    if let Some(path) = explicit {
        return Ok(path.clone());
    }
    default_path(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}
fn default_path(
    config: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf, Error> {
    let base = if let Some(path) = config.filter(|p| !p.is_empty()) {
        PathBuf::from(path)
    } else {
        PathBuf::from(home.filter(|p| !p.is_empty()).ok_or(Error::InvalidPath)?).join(".config")
    };
    if !base.is_absolute() {
        return Err(Error::InvalidPath);
    }
    Ok(base.join("franken_remote/hosts.json"))
}
fn store(options: &SavedHostOptions) -> Result<Store, Failure> {
    Store::new(&path(options.file.as_ref()).map_err(failure)?).map_err(failure)
}
/// List/forget work with the installed daemon stopped and create no runtime.
pub(super) fn local(options: &SavedHostOptions, json: bool) -> Result<Option<String>, Failure> {
    match &options.action {
        SavedHostAction::Save { .. } => Ok(None),
        SavedHostAction::List => Ok(Some(render(
            &store(options)?.load().map_err(failure)?,
            "list",
            None,
            json,
        ))),
        SavedHostAction::Forget { alias } => {
            let saved = store(options)?
                .update(Change::Forget(alias.clone()))
                .map_err(failure)?;
            Ok(Some(render(
                &saved.peers,
                "forget",
                Some(saved.durable),
                json,
            )))
        }
    }
}
/// Called only after the original two-read daemon lookup has selected this ID.
pub(super) fn save(
    options: &SavedHostOptions,
    stable_id: &str,
    name: &str,
    json: bool,
) -> Result<String, Failure> {
    let SavedHostAction::Save { alias, node, port } = &options.action else {
        return Err(failure(Error::Data(SavedPeerError::InvalidRecord)));
    };
    if stable_id != node {
        return Err(failure(Error::Data(SavedPeerError::InvalidRecord)));
    }
    let peer = SavedPeer::new(alias.clone(), stable_id.to_owned(), name.to_owned(), *port)
        .map_err(|e| failure(e.into()))?;
    let saved = store(options)?.update(Change::Add(peer)).map_err(failure)?;
    Ok(render(&saved.peers, "save", Some(saved.durable), json))
}
fn render(peers: &SavedPeers, action: &str, durable: Option<bool>, json: bool) -> String {
    if json {
        let hosts =
            serde_json::to_string(peers.list()).expect("validated string and integer fields");
        format!(
            "{{\"schema_version\":1,\"timestamp_unix_ms\":{},\"outcome\":\"success\",\"action\":{},\"changed\":{},\"durable\":{},\"selection_only\":true,\"fresh_lookup_required\":true,\"hosts\":{hosts}}}\n",
            output::timestamp(),
            output::quoted(action),
            durable.is_some(),
            durable.map_or("null", |v| if v { "true" } else { "false" })
        )
    } else {
        let mut out = String::from(
            "Saved hosts (local selection metadata; every connection rechecks identity and authorization):\n",
        );
        for peer in peers.list() {
            let _ = writeln!(
                out,
                "{}  {}  {}:{}",
                peer.alias(),
                peer.stable_id(),
                peer.certificate_name(),
                peer.port()
            );
        }
        if peers.list().is_empty() {
            out.push_str("No saved hosts.\n");
        }
        if durable == Some(false) {
            out.push_str(
                "The change was published, but crash durability could not be confirmed.\n",
            );
        }
        out
    }
}
/// Owned selection keeps borrowed selector fields alive across the connection.
pub(super) struct Selection {
    pub node: String,
    pub port: u16,
    by_name: bool,
    name: Option<String>,
}
impl Selection {
    pub(super) fn selector(&self) -> fr_tailnet::PeerSelector<'_> {
        if let Some(name) = &self.name {
            fr_tailnet::PeerSelector::Pinned {
                stable_id: &self.node,
                certificate_name: name,
            }
        } else if self.by_name {
            fr_tailnet::PeerSelector::Name(&self.node)
        } else {
            fr_tailnet::PeerSelector::StableId(&self.node)
        }
    }
}
pub(super) fn select(target: &Target) -> Result<Selection, Failure> {
    if let Some(saved) = &target.saved {
        let peers = Store::new(&path(saved.file.as_ref()).map_err(failure)?)
            .map_err(failure)?
            .load()
            .map_err(failure)?;
        let peer = peers
            .get(&target.node)
            .ok_or_else(|| failure(Error::Data(SavedPeerError::NotFound)))?;
        Ok(Selection {
            node: peer.stable_id().to_owned(),
            port: peer.port(),
            by_name: false,
            name: Some(peer.certificate_name().to_owned()),
        })
    } else {
        Ok(Selection {
            node: target.node.clone(),
            port: target.port,
            by_name: target.by_name,
            name: None,
        })
    }
}

#[cfg(test)]
#[path = "saved_hosts/tests.rs"]
mod tests;
