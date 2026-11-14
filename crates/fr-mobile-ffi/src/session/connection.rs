//! Immutable destination selection for the shared viewer and directory client.
//!
//! A local discovery row is comparison metadata, never dial authority. Every
//! installed connection resolves its original node ID and certificate name
//! again through the authenticated daemon before dialing or starting media.
#![forbid(unsafe_code)]

use fr_client::discovery::picker::{Candidate, Catalog, MAX_CANDIDATES, PathHint};
use fr_tailnet::{Discovery, Error, HostName, HostPin, PeerSelector, PeerTransportPath};
use frd::native_connection::{Client, Error as ConnectionError};
use std::{fmt, net::IpAddr, time::Duration};

pub use frd::native_connection::AddressFamily;

#[derive(Clone)]
enum Route {
    Named {
        host: HostName,
        pin: Option<HostPin>,
        direct: bool,
    },
    Installed {
        peer: Candidate,
        addresses: Box<[IpAddr]>,
        family: AddressFamily,
    },
}

/// One immutable destination. Cloning retains the original choice; it never
/// adopts another discovery row or grants observation, control or attachment.
#[derive(Clone)]
pub struct Target(Route);

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Target([destination selection])")
    }
}

impl Target {
    /// A destination for platforms without a readable installed daemon. A
    /// direct directory pin must name exactly this host.
    pub fn named(host: HostName, pin: Option<HostPin>, direct: bool) -> Result<Self, Error> {
        if pin.as_ref().is_some_and(|pin| pin.name() != &host) || (direct && pin.is_none()) {
            return Err(Error::InvalidEndpoint);
        }
        Ok(Self(Route::Named { host, pin, direct }))
    }

    /// Select exactly one stable ID or canonical name from one bounded local
    /// snapshot. Names accept ASCII case and one trailing DNS dot; stable IDs
    /// remain exact. Ambiguous or malformed snapshots refuse the whole choice.
    ///
    /// Explicit addresses constrain the COMPLETE node-owned address set. Their
    /// order is immaterial, but subsets, duplicates and foreign addresses fail.
    /// The same constraint is checked against both fresh daemon reads at dial.
    /// Use `family` to choose IPv4 or IPv6 without replacing identity metadata.
    pub fn installed(
        snapshot: &Discovery,
        selection: &str,
        addresses: Vec<IpAddr>,
        family: AddressFamily,
    ) -> Result<Self, Error> {
        if snapshot.peers().len() > MAX_CANDIDATES {
            return Err(Error::MalformedMetadata);
        }
        if addresses.len() > 8 {
            return Err(Error::InvalidEndpoint);
        }
        let candidates = snapshot
            .peers()
            .iter()
            .map(|peer| {
                let hint = match peer.transport_path() {
                    PeerTransportPath::Direct { .. } => PathHint::Direct,
                    PeerTransportPath::DerpRelayed { .. } => PathHint::Relayed,
                    PeerTransportPath::Unknown => PathHint::Unknown,
                };
                Candidate::new(
                    peer.stable_id().to_owned(),
                    peer.certificate_name().to_owned(),
                    hint,
                )
                .map_err(|_| Error::MalformedMetadata)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let catalog = Catalog::new(candidates).map_err(|_| Error::MalformedMetadata)?;
        let name = selection.strip_suffix('.').unwrap_or(selection);
        let mut matches = catalog.candidates().iter().filter(|peer| {
            peer.stable_id() == selection || peer.certificate_name().eq_ignore_ascii_case(name)
        });
        let peer = matches.next().ok_or(Error::IdentityMismatch)?;
        if matches.next().is_some() {
            return Err(Error::IdentityMismatch);
        }
        if !addresses.is_empty() {
            let original = snapshot
                .peers()
                .iter()
                .find(|original| original.stable_id() == peer.stable_id())
                .ok_or(Error::IdentityMismatch)?;
            if addresses.len() != original.addresses().len()
                || addresses.iter().enumerate().any(|(index, address)| {
                    addresses[..index].contains(address) || !original.addresses().contains(address)
                })
            {
                return Err(Error::AddressMismatch);
            }
        }
        Ok(Self(Route::Installed {
            peer: peer.clone(),
            addresses: addresses.into_boxed_slice(),
            family,
        }))
    }

    pub fn certificate_name(&self) -> &str {
        match &self.0 {
            Route::Named { host, .. } => host.as_str(),
            Route::Installed { peer, .. } => peer.certificate_name(),
        }
    }

    pub(crate) fn selector(&self) -> Option<PeerSelector<'_>> {
        match &self.0 {
            Route::Named { .. } => None,
            Route::Installed {
                peer, addresses, ..
            } if !addresses.is_empty() => Some(PeerSelector::PinnedAddresses {
                stable_id: peer.stable_id(),
                certificate_name: peer.certificate_name(),
                addresses,
            }),
            Route::Installed { peer, .. } => Some(PeerSelector::Pinned {
                stable_id: peer.stable_id(),
                certificate_name: peer.certificate_name(),
            }),
        }
    }

    pub(crate) fn family(&self) -> AddressFamily {
        match &self.0 {
            Route::Named { .. } => AddressFamily::Automatic,
            Route::Installed { family, .. } => *family,
        }
    }

    pub(crate) fn client(&self) -> Result<Client, ConnectionError> {
        let roots = crate::roots::bundled().map_err(ConnectionError::Tailnet)?;
        let timeout = Duration::from_secs(5);
        match &self.0 {
            Route::Named {
                host,
                pin,
                direct: false,
            } => Client::named(host.clone(), pin.clone(), roots, timeout),
            Route::Named {
                pin: Some(pin),
                direct: true,
                ..
            } => Client::named_at(pin.clone(), roots, timeout),
            Route::Named { .. } => Err(ConnectionError::InvalidConfiguration),
            Route::Installed { .. } => {
                #[cfg(any(target_os = "macos", windows))]
                {
                    Client::new(fr_tailnet::LocalApi::installed(), roots, timeout)
                }
                #[cfg(not(any(target_os = "macos", windows)))]
                {
                    Err(ConnectionError::Tailnet(Error::LocalApiUnavailable))
                }
            }
        }
    }
}

/// Read one bounded, authenticated installed-daemon snapshot on the calling
/// thread. It starts no remote probes and says nothing about desktop readiness.
#[cfg(any(target_os = "macos", windows))]
pub fn discover() -> Result<Discovery, Error> {
    use asupersync::{runtime::RuntimeBuilder, types::Budget};

    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .map_err(|_| Error::MissingRuntime)?;
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let api = fr_tailnet::LocalApi::installed();
    runtime.block_on(api.discover(&cx))
}

#[cfg(test)]
mod tests;
