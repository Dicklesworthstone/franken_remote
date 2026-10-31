//! Local aliases pinned to a stable peer and its canonical TLS name.
//!
//! Selection metadata only: every connection must resolve both values against
//! fresh installed-daemon metadata. Addresses, permissions, credentials and
//! session handles are deliberately absent from this format.
use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAX_SAVED_PEERS: usize = 100;
pub const MAX_SAVED_PEER_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedPeerError {
    InvalidRecord,
    InvalidDocument,
    TooLarge,
    Capacity,
    Duplicate,
    NotFound,
}
impl fmt::Display for SavedPeerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deserializer diagnostics may contain arbitrary local document text.
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SavedPeerError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SavedPeer {
    alias: String,
    stable_id: String,
    certificate_name: String,
    port: u16,
}
impl SavedPeer {
    pub fn new(
        alias: String,
        stable_id: String,
        certificate_name: String,
        port: u16,
    ) -> Result<Self, SavedPeerError> {
        if !valid_alias(&alias)
            || !valid_id(&stable_id)
            || !valid_name(&certificate_name)
            || port == 0
        {
            return Err(SavedPeerError::InvalidRecord);
        }
        Ok(Self {
            alias,
            stable_id,
            certificate_name,
            port,
        })
    }
    pub fn alias(&self) -> &str {
        &self.alias
    }
    pub fn stable_id(&self) -> &str {
        &self.stable_id
    }
    pub fn certificate_name(&self) -> &str {
        &self.certificate_name
    }
    pub const fn port(&self) -> u16 {
        self.port
    }
}
/// An explicit local alias. It never shadows a node ID without saved-host mode.
pub fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 64
        && alias.as_bytes()[0].is_ascii_alphanumeric()
        && alias
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.is_ascii()
        && !id.starts_with('-')
        && !id
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace() || b"/:?#@\\".contains(&b))
}
fn valid_name(name: &str) -> bool {
    name.len() <= 253
        && name.contains('.')
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedPeers {
    hosts: Vec<SavedPeer>,
}
impl SavedPeers {
    pub fn list(&self) -> &[SavedPeer] {
        &self.hosts
    }
    pub fn get(&self, alias: &str) -> Option<&SavedPeer> {
        self.hosts.iter().find(|p| p.alias == alias)
    }
    pub fn add(&mut self, peer: SavedPeer) -> Result<(), SavedPeerError> {
        if self.hosts.iter().any(|p| {
            p.alias == peer.alias
                || (p.stable_id == peer.stable_id && p.port == peer.port)
                || (p.certificate_name == peer.certificate_name && p.port == peer.port)
        }) {
            return Err(SavedPeerError::Duplicate);
        }
        if self.hosts.len() == MAX_SAVED_PEERS {
            return Err(SavedPeerError::Capacity);
        }
        self.hosts.push(peer);
        self.hosts.sort_by(|a, b| a.alias.cmp(&b.alias));
        Ok(())
    }
    pub fn remove(&mut self, alias: &str) -> Result<(), SavedPeerError> {
        let i = self
            .hosts
            .iter()
            .position(|p| p.alias == alias)
            .ok_or(SavedPeerError::NotFound)?;
        self.hosts.remove(i);
        Ok(())
    }
    pub fn to_json(&self) -> Result<Vec<u8>, SavedPeerError> {
        #[derive(Serialize)]
        struct Document<'a> {
            schema_version: u8,
            hosts: &'a [SavedPeer],
        }
        let mut bytes = serde_json::to_vec_pretty(&Document {
            schema_version: 1,
            hosts: &self.hosts,
        })
        .map_err(|_| SavedPeerError::InvalidDocument)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_SAVED_PEER_BYTES {
            return Err(SavedPeerError::TooLarge);
        }
        Ok(bytes)
    }
    pub fn from_json(bytes: &[u8]) -> Result<Self, SavedPeerError> {
        if bytes.len() > MAX_SAVED_PEER_BYTES {
            return Err(SavedPeerError::TooLarge);
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Record {
            alias: String,
            stable_id: String,
            certificate_name: String,
            port: u16,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Document {
            schema_version: u8,
            hosts: Vec<Record>,
        }
        let doc: Document =
            serde_json::from_slice(bytes).map_err(|_| SavedPeerError::InvalidDocument)?;
        if doc.schema_version != 1 {
            return Err(SavedPeerError::InvalidDocument);
        }
        if doc.hosts.len() > MAX_SAVED_PEERS {
            return Err(SavedPeerError::Capacity);
        }
        let mut peers = Self::default();
        for record in doc.hosts {
            peers.add(SavedPeer::new(
                record.alias,
                record.stable_id,
                record.certificate_name,
                record.port,
            )?)?;
        }
        Ok(peers)
    }
}

#[cfg(test)]
mod tests;
