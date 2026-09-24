#![forbid(unsafe_code)]
//! Credential regressions against the real policy managers. These do not claim
//! a browser listener, installed-tailnet admission or timing/hardware qualification.
use fr_core::{ids::RemoteSessionId, time::HostInstant};
use frd::broker::browser_security::{
    AuxiliaryChannelRole as Channel, AuxiliaryTicketManager, AuxiliaryTicketRefusal,
    BOOTSTRAP_NONCE_TTL, BootstrapNonceManager, BrowserSessionRole as Role,
    MAX_PENDING_NONCES_GLOBAL, MAX_PENDING_NONCES_PER_PEER, MAX_PENDING_TICKETS_GLOBAL,
    NonceRefusal,
};
use std::net::{IpAddr, Ipv4Addr};

fn peer() -> IpAddr {
    "100.64.10.20".parse().unwrap()
}
fn now() -> HostInstant {
    HostInstant::from_micros(1_000)
}
fn id() -> RemoteSessionId {
    RemoteSessionId::from_raw(51)
}

#[test]
fn identical_public_inputs_in_fresh_managers_do_not_reproduce_credentials() {
    let n1 = BootstrapNonceManager::new()
        .issue_nonce(peer(), id(), Role::Observer, now())
        .unwrap();
    let n2 = BootstrapNonceManager::new()
        .issue_nonce(peer(), id(), Role::Observer, now())
        .unwrap();
    assert_ne!(n1, n2, "public metadata must not determine a bearer nonce");
    let t1 = AuxiliaryTicketManager::new()
        .issue_ticket(id(), Channel::ClipboardSync, peer(), now())
        .unwrap();
    let t2 = AuxiliaryTicketManager::new()
        .issue_ticket(id(), Channel::ClipboardSync, peer(), now())
        .unwrap();
    assert_ne!(
        t1, t2,
        "public metadata must not determine an attachment ticket"
    );
}

#[test]
fn rejected_bindings_leave_the_original_tokens_available_once() {
    let wrong = "100.64.10.21".parse().unwrap();
    let mut nonces = BootstrapNonceManager::new();
    let nonce = nonces
        .issue_nonce(peer(), id(), Role::Observer, now())
        .unwrap();
    assert_eq!(
        nonces.consume_nonce(nonce.as_bytes(), wrong, Role::Observer, now()),
        Err(NonceRefusal::PeerIpMismatch)
    );
    assert_eq!(
        nonces.consume_nonce(nonce.as_bytes(), peer(), Role::Observer, now()),
        Ok(id())
    );
    assert_eq!(
        nonces.consume_nonce(nonce.as_bytes(), peer(), Role::Observer, now()),
        Err(NonceRefusal::NonceNotFoundOrConsumed)
    );
    let mut tickets = AuxiliaryTicketManager::new();
    let ticket = tickets
        .issue_ticket(id(), Channel::FilesTransfer, peer(), now())
        .unwrap();
    assert_eq!(
        tickets.consume_ticket(
            Some(ticket.as_bytes()),
            id(),
            Channel::FilesTransfer,
            wrong,
            now()
        ),
        Err(AuxiliaryTicketRefusal::PeerIpMismatch)
    );
    assert_eq!(
        tickets.consume_ticket(
            Some(ticket.as_bytes()),
            id(),
            Channel::FilesTransfer,
            peer(),
            now()
        ),
        Ok(())
    );
    assert_eq!(
        tickets.consume_ticket(
            Some(ticket.as_bytes()),
            id(),
            Channel::FilesTransfer,
            peer(),
            now()
        ),
        Err(AuxiliaryTicketRefusal::TicketNotFoundOrConsumed)
    );
}

#[test]
fn debug_of_populated_managers_does_not_expose_raw_tokens_or_peer_bindings() {
    let mut nonces = BootstrapNonceManager::new();
    let nonce = nonces
        .issue_nonce(peer(), id(), Role::Controller, now())
        .unwrap();
    let mut tickets = AuxiliaryTicketManager::new();
    let ticket = tickets
        .issue_ticket(id(), Channel::AudioUplink, peer(), now())
        .unwrap();
    for (debug, secret) in [
        (format!("{nonces:?}"), nonce),
        (format!("{tickets:?}"), ticket),
    ] {
        assert!(!debug.contains(&format!("{:?}", secret.as_bytes())));
        assert!(!debug.contains(&peer().to_string()));
        assert!(debug.contains("pending: 1"));
    }
}

#[test]
fn peer_global_and_ticket_limits_survive_mismatches_and_expiry() {
    let mut nonces = BootstrapNonceManager::new();
    let mut first = None;
    for index in 0..MAX_PENDING_NONCES_GLOBAL {
        let peer_id = u8::try_from(index / MAX_PENDING_NONCES_PER_PEER).unwrap();
        let address = IpAddr::V4(Ipv4Addr::new(100, 64, 20, peer_id));
        let token = nonces
            .issue_nonce(address, id(), Role::Observer, now())
            .unwrap();
        if index == 0 {
            first = Some((address, token));
        }
        if index == MAX_PENDING_NONCES_PER_PEER - 1 {
            assert_eq!(
                nonces.issue_nonce(address, id(), Role::Observer, now()),
                Err(NonceRefusal::RateLimitExceeded)
            );
        }
    }
    assert_eq!(
        nonces.issue_nonce(peer(), id(), Role::Observer, now()),
        Err(NonceRefusal::NonceCapacityExceeded)
    );
    let (address, token) = first.unwrap();
    assert_eq!(
        nonces.consume_nonce(token.as_bytes(), peer(), Role::Observer, now()),
        Err(NonceRefusal::PeerIpMismatch)
    );
    assert_eq!(nonces.pending_count(), MAX_PENDING_NONCES_GLOBAL);
    assert_eq!(
        nonces.consume_nonce(token.as_bytes(), address, Role::Observer, now()),
        Ok(id())
    );
    assert!(
        nonces
            .issue_nonce(address, id(), Role::Observer, now())
            .is_ok()
    );
    let expires = now().checked_add(BOOTSTRAP_NONCE_TTL).unwrap();
    nonces.sweep_expired(expires);
    assert_eq!(nonces.pending_count(), 0);
    let mut tickets = AuxiliaryTicketManager::new();
    for _ in 0..MAX_PENDING_TICKETS_GLOBAL {
        tickets
            .issue_ticket(id(), Channel::AudioDownlink, peer(), now())
            .unwrap();
    }
    assert_eq!(
        tickets.issue_ticket(id(), Channel::AudioDownlink, peer(), now()),
        Err(AuxiliaryTicketRefusal::TicketCapacityExceeded)
    );
    tickets.sweep_expired(expires);
    assert_eq!(tickets.pending_count(), 0);
    assert!(
        tickets
            .issue_ticket(id(), Channel::AudioDownlink, peer(), expires)
            .is_ok()
    );
}
