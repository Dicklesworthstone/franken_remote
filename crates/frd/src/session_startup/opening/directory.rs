//! The terminal tailnet directory (`frd run --directory`, `PROTOCOL_DIRECTORY.md`).
//! After the one recognized request: fence the startup (no session, approval,
//! observation or input can follow), refresh the admission, read the directory
//! once from the installed daemon, and send the listing, or a typed
//! `DirectoryUnavailable` refusal. Like inspection, it never selects a session
//! and ends the connection as `InspectionComplete`.
use super::super::{Error, Host, Peer, Role, now};
use super::refusal;
use asupersync::time::timeout;
use fr_transport::quic::{self, Route};
use fr_wire::{
    directory::{self, Entry, Listing},
    refusal::Reason,
};
use std::time::Duration;

/// Once every record is acknowledged, give the viewer this long to close
/// first, so its last reads never race this host's close.
const LINGER_US: u64 = 1_000_000;
const MAX_TURNS: usize = 4 * directory::MAX_RECORDS;

pub(super) async fn report(host: &mut Host) -> Result<(), Error> {
    // Fence before the first await, exactly as inspection does.
    refusal::fence(host);
    let result = serve(host).await;
    host.close();
    result
}

async fn serve(host: &mut Host) -> Result<(), Error> {
    let records = read(host).await.and_then(|directory| {
        directory::encode_listing(&listing(&directory)?, host.maximum)
            .map_err(|_| Error::DirectoryUnavailable)
    });
    match records {
        Ok(records) => send(host, &records).await,
        Err(error) => {
            // Best effort, under the same admission; the original cause stays.
            let _ = refusal::send(host, Reason::DirectoryUnavailable).await;
            Err(error)
        }
    }
}

/// A fresh admission first (the proof lives about a second), then one read.
async fn read(host: &mut Host) -> Result<fr_tailnet::Directory, Error> {
    match host.peer.as_mut().ok_or(Error::Closed)? {
        Peer::Tailnet(owner) => {
            owner.refresh().await.map_err(Error::Admission)?;
            owner.directory().await.map_err(Error::Admission)
        }
        #[cfg(test)]
        Peer::Fixture { .. } => host
            .directory_fixture
            .clone()
            .ok_or(Error::DirectoryUnavailable)?
            .map_err(Error::Admission),
    }
}

/// The wire listing. A machine the wire cannot carry exactly is left out and
/// the listing says it is incomplete; the host itself must be listable.
fn listing(directory: &fr_tailnet::Directory) -> Result<Listing, Error> {
    let mut listing = Listing {
        entries: Vec::with_capacity(directory.nodes.len()),
        truncated: directory.truncated,
    };
    for node in &directory.nodes {
        let entry = Entry {
            name: node.name.clone(),
            node_id: node.node_id.clone(),
            os: node.os.clone(),
            online: node.online,
            serving: node.serving,
            ipv4: node.ipv4,
            ipv6: node.ipv6,
        };
        match entry.validate() {
            Ok(()) => listing.entries.push(entry),
            Err(_) if node.serving => return Err(Error::DirectoryUnavailable),
            Err(_) => listing.truncated = true,
        }
    }
    if !listing.entries.first().is_some_and(|e| e.serving) {
        return Err(Error::DirectoryUnavailable);
    }
    Ok(listing)
}

async fn send(host: &mut Host, records: &[Vec<u8>]) -> Result<(), Error> {
    let cx = &host.cx;
    let started = now(cx)?;
    if started < host.last {
        return Err(Error::Clock);
    }
    let peer = host.peer.as_ref().ok_or(Error::Closed)?;
    let until = host.until.min(peer.check(cx, Role::Observe)?);
    let transport = host.transport.as_mut().ok_or(Error::Closed)?;
    peer.check_addresses(transport)?;
    let route = host.routes.outbound;
    let inbound = host.routes.inbound;
    if route.binding != 0 || route.messages != quic::Messages::Negotiation {
        return Err(Error::Order);
    }
    let allowed = || {
        now(cx).is_ok_and(|at| at >= started && at < until) && peer.check(cx, Role::Observe).is_ok()
    };
    let remaining = until.checked_sub(now(cx)?).ok_or(Error::Expired)?;
    // The same connection, its original record deadlines and lifetime guard;
    // no receive is dispatched in this terminal scope.
    let drain = async {
        let mut next = 0;
        for _ in 0..MAX_TURNS {
            while next < records.len() {
                match transport.send(cx, Route::Stream(route), &records[next], until, allowed) {
                    Ok(()) => next += 1,
                    Err(quic::Error::Backpressure) => break,
                    Err(error) => return Err(error),
                }
            }
            transport
                .drive(cx, Duration::from_millis(10), allowed)
                .await?;
            if next == records.len() && transport.usage().retained_send_records == 0 {
                return Ok(());
            }
        }
        Err(quic::Error::Backpressure)
    };
    timeout(cx.now(), Duration::from_micros(remaining), drain)
        .await
        .map_err(|_| Error::Expired)?
        .map_err(Error::from)?;
    // Delivered to the viewer's transport. Wait (bounded) for its close.
    let linger_until = now(cx)?.saturating_add(LINGER_US).min(until);
    let open =
        || now(cx).is_ok_and(|at| at < linger_until) && peer.check(cx, Role::Observe).is_ok();
    let linger = async {
        while open() {
            if transport
                .drive(cx, Duration::from_millis(10), open)
                .await
                .is_err()
                || transport.receive_ended(inbound).unwrap_or(true)
            {
                break;
            }
        }
    };
    let remaining = linger_until.saturating_sub(now(cx)?);
    let _ = timeout(cx.now(), Duration::from_micros(remaining), linger).await;
    Ok(())
}

#[cfg(test)]
mod tests;
