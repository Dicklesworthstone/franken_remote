//! The serving host's directory of the VIEWER's own machines (`frd run
//! --directory`), from one installed-daemon status read after admission.
//!
//! Listed: the host itself, then every peer of the viewer's own Tailscale
//! user that is untagged, not shared in or out, unexpired, in the network map,
//! and named under this tailnet's `MagicDNS` suffix. Never listed: the viewer's
//! own node, other users' and tagged nodes, nodes from other tailnets, and
//! anything at all for a tagged viewer except the host. Names, IDs and
//! addresses come only from the daemon; nothing is inferred or invented.
//!
//! The status body is already bounded (`STATUS_BYTES`, 1024 peers). One odd
//! peer (an unusual name, ID or OS label) is left out, not fatal.
use crate::{Error, metadata::Text};
use serde::{Deserialize, de::IgnoredAny};
use std::{
    collections::BTreeMap,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

/// At most this many machines (the host included) per directory.
pub const MAX_NODES: usize = 256;
const MAX_NAME: usize = 253;
const MAX_LABEL: usize = 63;
const MAX_NODE_ID: usize = 64;
const MAX_OS: usize = 32;

/// One listed machine. Debug output never shows its name, ID or addresses.
#[derive(Clone, PartialEq, Eq)]
pub struct DirectoryNode {
    /// `MagicDNS` name, lowercase, without the trailing dot.
    pub name: String,
    /// Tailscale's stable node ID.
    pub node_id: String,
    /// Tailscale's OS label; empty when absent or unusual.
    pub os: String,
    /// Tailscale's `Online` flag (the serving host: true).
    pub online: bool,
    /// The host serving this directory.
    pub serving: bool,
    pub ipv4: Option<Ipv4Addr>,
    pub ipv6: Option<Ipv6Addr>,
}
impl fmt::Debug for DirectoryNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectoryNode")
            .field("online", &self.online)
            .field("serving", &self.serving)
            .finish_non_exhaustive()
    }
}

/// The serving host first, then the viewer's machines by name.
#[derive(Clone, PartialEq, Eq)]
pub struct Directory {
    pub nodes: Vec<DirectoryNode>,
    /// More machines matched than [`MAX_NODES`].
    pub truncated: bool,
}
impl fmt::Debug for Directory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Directory")
            .field("nodes", &self.nodes.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

#[derive(Deserialize)]
struct Tailnet {
    #[serde(rename = "MagicDNSSuffix", default)]
    suffix: Option<String>,
}
#[derive(Deserialize)]
#[allow(clippy::struct_excessive_bools)] // Mirrors LocalAPI PeerStatus fields 1:1.
struct Node {
    #[serde(rename = "ID", default)]
    id: Option<String>,
    #[serde(rename = "DNSName", default)]
    dns_name: Option<String>,
    #[serde(rename = "OS", default)]
    os: Option<String>,
    #[serde(rename = "UserID", default)]
    user: u64,
    #[serde(rename = "TailscaleIPs", default)]
    ips: Option<Vec<IpAddr>>,
    #[serde(rename = "Tags", default)]
    tags: Option<Vec<IgnoredAny>>,
    #[serde(rename = "AltSharerUserID", default)]
    sharer: u64,
    #[serde(rename = "ShareeNode", default)]
    sharee: bool,
    #[serde(rename = "Expired", default)]
    expired: bool,
    #[serde(rename = "InNetworkMap", default)]
    in_map: bool,
    #[serde(rename = "Online", default)]
    online: bool,
}
/// The status subset the directory reads.
#[derive(Deserialize)]
pub(crate) struct Status {
    #[serde(rename = "BackendState")]
    backend: Text<32>,
    #[serde(rename = "Self")]
    this: Node,
    #[serde(rename = "CurrentTailnet", default)]
    tailnet: Option<Tailnet>,
    #[serde(rename = "Peer", deserialize_with = "crate::metadata::peers", default)]
    peers: BTreeMap<String, Node>,
}
impl Status {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > crate::metadata::STATUS_BYTES {
            return Err(Error::MalformedMetadata);
        }
        serde_json::from_slice(bytes).map_err(|_| Error::MalformedMetadata)
    }
}

/// Who asked, from the connection's admission proof.
#[derive(Clone, Copy)]
pub(crate) struct Scope<'a> {
    /// The serving host's stable node ID when admitted.
    pub host: &'a str,
    /// The viewer's stable node ID (never listed).
    pub viewer: &'a str,
    /// The viewer's user ID; 0 for a tagged viewer, which owns no machines.
    pub user: u64,
}

fn label_ok(label: &str) -> bool {
    let bytes = label.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_LABEL
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}
fn tailnet_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..128).contains(&b)
}
fn tailnet_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
}
/// This node as listed, or None when it cannot be listed exactly.
fn listed(node: &Node, suffix: &str, serving: bool) -> Option<DirectoryNode> {
    let name = node
        .dns_name
        .as_deref()?
        .strip_suffix('.')?
        .to_ascii_lowercase();
    // Exactly one machine label directly under this tailnet's suffix.
    let machine = name.strip_suffix(suffix)?.strip_suffix('.')?;
    if name.len() > MAX_NAME || !label_ok(machine) {
        return None;
    }
    let id = node.id.as_deref()?;
    if id.is_empty()
        || id.len() > MAX_NODE_ID
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    let os = node
        .os
        .as_deref()
        .filter(|os| {
            os.len() <= MAX_OS
                && os
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'.' | b'-' | b'_'))
        })
        .unwrap_or_default();
    let ips = node.ips.as_deref().unwrap_or_default();
    let ipv4 = ips.iter().find_map(|ip| match ip {
        IpAddr::V4(v4) if tailnet_v4(*v4) => Some(*v4),
        _ => None,
    });
    let ipv6 = ips.iter().find_map(|ip| match ip {
        IpAddr::V6(v6) if tailnet_v6(*v6) => Some(*v6),
        _ => None,
    });
    if ipv4.is_none() && ipv6.is_none() {
        return None;
    }
    Some(DirectoryNode {
        name,
        node_id: id.to_owned(),
        os: os.to_owned(),
        online: serving || node.online,
        serving,
        ipv4,
        ipv6,
    })
}

/// The directory `scope` may see in `status`. The host itself must be
/// listable and still be the admitted host.
pub(crate) fn select(status: &Status, scope: Scope<'_>) -> Result<Directory, Error> {
    if status.backend.0 != "Running" {
        return Err(Error::BackendNotRunning);
    }
    if status.this.id.as_deref() != Some(scope.host) {
        return Err(Error::IdentityChanged);
    }
    let suffix = status
        .tailnet
        .as_ref()
        .and_then(|t| t.suffix.as_deref())
        .map(|s| s.trim_end_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty() && s.split('.').all(label_ok))
        .ok_or(Error::MalformedMetadata)?;
    let host = listed(&status.this, &suffix, true).ok_or(Error::MalformedMetadata)?;
    let mut nodes: Vec<DirectoryNode> = Vec::new();
    if scope.user != 0 {
        for peer in status.peers.values() {
            let own = peer.user == scope.user
                && peer.tags.as_ref().is_none_or(Vec::is_empty)
                && peer.sharer == 0
                && !peer.sharee
                && !peer.expired
                && peer.in_map
                && peer.id.as_deref().is_some_and(|id| id != scope.viewer);
            if let Some(node) = own.then(|| listed(peer, &suffix, false)).flatten()
                && node.node_id != host.node_id
                && node.name != host.name
            {
                nodes.push(node);
            }
        }
    }
    nodes.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.node_id.cmp(&b.node_id)));
    // A name or ID twice is a daemon inconsistency: list neither copy.
    let duplicate =
        |a: &DirectoryNode, b: &DirectoryNode| a.name == b.name || a.node_id == b.node_id;
    let unique: Vec<DirectoryNode> = nodes
        .iter()
        .filter(|n| nodes.iter().filter(|m| duplicate(n, m)).count() == 1)
        .cloned()
        .collect();
    let truncated = unique.len() > MAX_NODES - 1;
    let mut listed_nodes = Vec::with_capacity(unique.len().min(MAX_NODES - 1) + 1);
    listed_nodes.push(host);
    listed_nodes.extend(unique.into_iter().take(MAX_NODES - 1));
    Ok(Directory {
        nodes: listed_nodes,
        truncated,
    })
}

#[cfg(test)]
mod tests;
