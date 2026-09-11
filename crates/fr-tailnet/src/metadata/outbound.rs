//! Resolve a connection destination, never a desktop permission or delegation.
use super::{HostIdentity, Peer, Status};
use crate::{Error, PeerSelector};

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub host: HostIdentity,
    pub peer: Peer,
    pub certificate_name: String,
}
pub(crate) fn resolve(status: &Status, selector: PeerSelector<'_>) -> Result<Target, Error> {
    let host = status.host_identity()?;
    let mut candidates = status.peers.iter().filter(|(_, peer)| match selector {
        PeerSelector::StableId(id) => peer.id.0 == id,
        PeerSelector::Name(name) => peer.dns_name.as_ref().is_some_and(|dns| {
            dns.0
                .trim_end_matches('.')
                .eq_ignore_ascii_case(name.strip_suffix('.').unwrap_or(name))
        }),
        PeerSelector::Address(ip) => peer.ips.0.contains(&ip),
    });
    let (key, peer) = candidates.next().ok_or(Error::IdentityMismatch)?;
    if candidates.next().is_some() {
        return Err(Error::IdentityMismatch);
    }
    peer.validate()?;
    if !peer.in_map
        || &peer.key.0 != key
        || peer.id == host.host.id
        || peer.node_id == host.host.node_id
        || peer.key == host.host.key
    {
        return Err(Error::IdentityMismatch);
    }
    if peer.ips.0.iter().any(|ip| {
        host.host.ips.0.contains(ip)
            || matches!(ip, std::net::IpAddr::V6(v) if v.to_ipv4_mapped().is_some())
    }) {
        return Err(Error::AddressMismatch);
    }
    // Conflicting aliases or node-owned addresses cannot choose another node
    // merely because BTreeMap iteration happens to visit one entry first.
    for (other_key, other) in &status.peers {
        if other_key != key
            && (other.id == peer.id
                || other.node_id == peer.node_id
                || other.key == peer.key
                || other.dns_name == peer.dns_name
                || other.ips.0.iter().any(|ip| peer.ips.0.contains(ip)))
        {
            return Err(Error::IdentityMismatch);
        }
    }
    let dns = peer.dns_name.as_ref().ok_or(Error::MalformedMetadata)?;
    let name = dns.0.strip_suffix('.').unwrap_or(&dns.0);
    let suffix = status
        .tailnet
        .suffix
        .0
        .strip_suffix('.')
        .unwrap_or(&status.tailnet.suffix.0);
    // Names come from authenticated LocalAPI and select TLS identity ONLY.
    // This suffix is not evidence of membership or permission to use a desktop.
    if name.len() > 253
        || name == suffix
        || !name.strip_suffix(suffix).is_some_and(|s| s.ends_with('.'))
        || !name.split('.').all(|s| {
            !s.is_empty()
                && s.len() <= 63
                && !s.starts_with('-')
                && !s.ends_with('-')
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
    {
        return Err(Error::MalformedMetadata);
    }
    Ok(Target {
        host,
        peer: peer.clone(),
        certificate_name: name.to_owned(),
    })
}
