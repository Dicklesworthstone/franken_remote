//! Bounded `LocalAPI` wire subset. Unknown ordinary fields are skipped; known
//! duplicate fields, duplicate map keys, and unknown grant restrictions refuse.
use crate::{ConnectionAddresses, DESKTOP_CAPABILITY, Error, GrantPolicy, Permissions, Scope};
use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use std::{collections::BTreeMap, fmt, net::IpAddr};

pub(crate) const STATUS_BYTES: usize = 1024 * 1024;
pub(crate) const WHOIS_BYTES: usize = 64 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct List<T, const N: usize>(pub Vec<T>);
impl<T, const N: usize> Default for List<T, N> {
    fn default() -> Self {
        Self(Vec::new())
    }
}
impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for List<T, N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Bounded<T, const N: usize>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for Bounded<T, N> {
            type Value = List<T, N>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bounded list")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while values.len() < N {
                    let Some(v) = a.next_element()? else {
                        return Ok(List(values));
                    };
                    values.push(v);
                }
                if a.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom("list limit"));
                }
                Ok(List(values))
            }
        }
        d.deserialize_seq(Bounded::<T, N>(std::marker::PhantomData))
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Text<const N: usize>(pub String);
impl<'de, const N: usize> Deserialize<'de> for Text<N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Bounded<const N: usize>;
        impl<const N: usize> Visitor<'_> for Bounded<N> {
            type Value = Text<N>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bounded text")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value.len() > N || value.chars().any(char::is_control) {
                    return Err(E::custom("text limit"));
                }
                Ok(Text(value.to_owned()))
            }
        }
        d.deserialize_str(Bounded::<N>)
    }
}

#[derive(Deserialize, Clone, PartialEq, Eq)]
pub(crate) struct Peer {
    #[serde(rename = "ID")]
    pub id: Text<128>,
    #[serde(rename = "NodeID")]
    pub node_id: u64,
    #[serde(rename = "PublicKey")]
    pub key: Text<80>,
    #[serde(rename = "UserID")]
    pub user: u64,
    #[serde(rename = "TailscaleIPs")]
    pub ips: List<IpAddr, 8>,
    #[serde(rename = "Tags", default)]
    pub tags: Option<List<Text<128>, 32>>,
    #[serde(rename = "AltSharerUserID", default)]
    pub sharer: u64,
    #[serde(rename = "ShareeNode", default)]
    pub sharee: bool,
    #[serde(rename = "Expired", default)]
    pub expired: bool,
    #[serde(rename = "KeyExpiry", default)]
    pub expiry: Option<Text<40>>,
    #[serde(rename = "InNetworkMap")]
    pub in_map: bool,
}
#[derive(Deserialize, Clone, PartialEq, Eq)]
pub(crate) struct Tailnet {
    #[serde(rename = "Name")]
    name: Text<256>,
    #[serde(rename = "MagicDNSSuffix")]
    suffix: Text<256>,
}
#[derive(Deserialize, PartialEq, Eq)]
pub(crate) struct Status {
    #[serde(rename = "Version")]
    pub version: Text<128>,
    #[serde(rename = "BackendState")]
    backend: Text<32>,
    #[serde(rename = "Self")]
    pub this: Peer,
    #[serde(rename = "CurrentTailnet")]
    pub tailnet: Tailnet,
    #[serde(rename = "TailscaleIPs")]
    ips: List<IpAddr, 8>,
    #[serde(rename = "Peer", deserialize_with = "peers")]
    pub peers: BTreeMap<String, Peer>,
}
fn peers<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, Peer>, D::Error> {
    struct Peers;
    impl<'de> Visitor<'de> for Peers {
        type Value = BTreeMap<String, Peer>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("bounded peer map")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
            let mut map = BTreeMap::new();
            while let Some(k) = a.next_key::<Text<80>>()? {
                if map.len() == 1024 || map.contains_key(&k.0) {
                    return Err(de::Error::custom("peer map limit/duplicate"));
                }
                map.insert(k.0, a.next_value()?);
            }
            Ok(map)
        }
    }
    d.deserialize_map(Peers)
}
#[derive(Deserialize)]
pub(crate) struct Node {
    #[serde(rename = "ID")]
    id: u64,
    #[serde(rename = "StableID")]
    stable_id: Text<128>,
    #[serde(rename = "Key")]
    key: Text<80>,
    #[serde(rename = "User")]
    user: u64,
    #[serde(rename = "Addresses")]
    addresses: List<Text<64>, 8>,
    #[serde(rename = "Tags", default)]
    tags: Option<List<Text<128>, 32>>,
    #[serde(rename = "Sharer", default)]
    sharer: u64,
    #[serde(rename = "Expired", default)]
    expired: bool,
    #[serde(rename = "KeyExpiry", default)]
    expiry: Option<Text<40>>,
    #[serde(rename = "MachineAuthorized")]
    authorized: Option<bool>,
    #[serde(rename = "IsJailed", default)]
    jailed: bool,
    #[serde(rename = "UnsignedPeerAPIOnly", default)]
    peer_api_only: bool,
}
#[derive(Deserialize)]
pub(crate) struct WhoIs {
    #[serde(rename = "Node")]
    pub node: Node,
    #[serde(rename = "CapMap", default)]
    grants: Grants,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    version: u8,
    observe: bool,
    control: bool,
}
#[derive(Default)]
struct Grants {
    desktop: Vec<Grant>,
}
impl<'de> Deserialize<'de> for Grants {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Caps;
        impl<'de> Visitor<'de> for Caps {
            type Value = Grants;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("capability map")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Grants::default())
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut names = Vec::new();
                let mut desktop = Vec::new();
                while let Some(name) = a.next_key::<Text<256>>()? {
                    if names.len() == 128 || names.contains(&name.0) {
                        return Err(de::Error::custom("capability map limit/duplicate"));
                    }
                    if name.0 == DESKTOP_CAPABILITY {
                        desktop = a.next_value::<List<Grant, 8>>()?.0;
                    } else {
                        a.next_value::<de::IgnoredAny>()?;
                    }
                    names.push(name.0);
                }
                Ok(Grants { desktop })
            }
        }
        d.deserialize_any(Caps)
    }
}
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    host: Peer,
    peer: Peer,
    version: Text<128>,
    tailnet: Tailnet,
}

fn valid_key(s: &str) -> bool {
    s.strip_prefix("nodekey:")
        .is_some_and(|v| v.len() == 64 && v.bytes().all(|c| c.is_ascii_hexdigit()))
}
fn tags(tags: Option<&List<Text<128>, 32>>) -> &[Text<128>] {
    tags.map_or(&[], |v| v.0.as_slice())
}
fn same_tags(a: Option<&List<Text<128>, 32>>, b: Option<&List<Text<128>, 32>>) -> bool {
    let (a, b) = (tags(a), tags(b));
    a.len() == b.len() && a.iter().all(|tag| b.contains(tag)) && b.iter().all(|tag| a.contains(tag))
}
impl Peer {
    fn validate(&self) -> Result<(), Error> {
        if self.expired {
            return Err(Error::KeyExpired);
        }
        if self.sharer != 0 || self.sharee {
            return Err(Error::SharedPeer);
        }
        if self.id.0.is_empty()
            || self.node_id == 0
            || !valid_key(&self.key.0)
            || self.ips.0.is_empty()
        {
            return Err(Error::MalformedMetadata);
        }
        for (i, ip) in self.ips.0.iter().enumerate() {
            if self.ips.0[..i].contains(ip)
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_loopback()
            {
                return Err(Error::MalformedMetadata);
            }
        }
        for (i, tag) in tags(self.tags.as_ref()).iter().enumerate() {
            if !tag.0.starts_with("tag:")
                || tag.0.len() <= 4
                || tags(self.tags.as_ref())[..i].contains(tag)
            {
                return Err(Error::MalformedMetadata);
            }
        }
        Ok(())
    }
}
impl Status {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > STATUS_BYTES {
            return Err(Error::MalformedMetadata);
        }
        serde_json::from_slice(bytes).map_err(|_| Error::MalformedMetadata)
    }
    pub fn validate(&self, endpoints: ConnectionAddresses) -> Result<(), Error> {
        if self.backend.0 != "Running" {
            return Err(Error::BackendNotRunning);
        }
        self.this.validate()?;
        if self.version.0.is_empty()
            || self.tailnet.name.0.is_empty()
            || self.tailnet.suffix.0.is_empty()
        {
            return Err(Error::MalformedMetadata);
        }
        if !self.this.ips.0.contains(&endpoints.local.ip())
            || !self.ips.0.contains(&endpoints.local.ip())
        {
            return Err(Error::AddressMismatch);
        }
        Ok(())
    }
}
impl WhoIs {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > WHOIS_BYTES {
            return Err(Error::MalformedMetadata);
        }
        serde_json::from_slice(bytes).map_err(|_| Error::MalformedMetadata)
    }
}
/// A grant is authoritative because it is a per-connection `CapMap` from the
/// locally authenticated daemon. Node `CapMap`/Capabilities and names NEVER grant.
pub(crate) fn evaluate(
    status: &Status,
    who: &WhoIs,
    endpoints: ConnectionAddresses,
    policy: GrantPolicy,
) -> Result<(Identity, Permissions, Vec<Option<String>>), Error> {
    status.validate(endpoints)?;
    let n = &who.node;
    let peer = status.peers.get(&n.key.0).ok_or(Error::IdentityMismatch)?;
    peer.validate()?;
    if n.expired {
        return Err(Error::KeyExpired);
    }
    if n.sharer != 0 {
        return Err(Error::SharedPeer);
    }
    if n.authorized != Some(true) {
        return Err(Error::MachineNotAuthorized);
    }
    if n.jailed || n.peer_api_only || !peer.in_map {
        return Err(Error::CapabilityDenied);
    }
    if n.stable_id != peer.id
        || n.id != peer.node_id
        || n.user != peer.user
        || n.key != peer.key
        || !same_tags(n.tags.as_ref(), peer.tags.as_ref())
        || peer.id == status.this.id
    {
        return Err(Error::IdentityMismatch);
    }
    let mut own_ips = Vec::new();
    for address in &n.addresses.0 {
        let (ip, bits) = address.0.split_once('/').ok_or(Error::MalformedMetadata)?;
        let ip: IpAddr = ip.parse().map_err(|_| Error::MalformedMetadata)?;
        if bits != if ip.is_ipv4() { "32" } else { "128" } || own_ips.contains(&ip) {
            return Err(Error::AddressMismatch);
        }
        own_ips.push(ip);
    }
    if !own_ips.contains(&endpoints.peer.ip())
        || own_ips.len() != peer.ips.0.len()
        || !own_ips.iter().all(|ip| peer.ips.0.contains(ip))
    {
        return Err(Error::AddressMismatch);
    }
    if policy.scope == Scope::OwnUser {
        if !tags(status.this.tags.as_ref()).is_empty() || status.this.user == 0 {
            return Err(Error::ExplicitScopeRequired);
        }
        if !tags(peer.tags.as_ref()).is_empty() || peer.user == 0 || peer.user != status.this.user {
            return Err(Error::ScopeDenied);
        }
    }
    let mut permissions = Permissions {
        observe: false,
        control: false,
    };
    for grant in &who.grants.desktop {
        if grant.version != 1 || (grant.control && !grant.observe) {
            return Err(Error::InvalidCapability);
        }
        permissions.observe |= grant.observe;
        permissions.control |= grant.control;
    }
    if !permissions.observe {
        return Err(Error::CapabilityDenied);
    }
    let expiries = [&status.this.expiry, &peer.expiry, &n.expiry]
        .iter()
        .map(|v| v.as_ref().map(|s| s.0.clone()))
        .collect();
    Ok((
        Identity {
            host: status.this.clone(),
            peer: peer.clone(),
            version: status.version.clone(),
            tailnet: status.tailnet.clone(),
        },
        permissions,
        expiries,
    ))
}
