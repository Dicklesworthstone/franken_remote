use super::super::{
    AuxiliaryChannelRole, AuxiliaryTicketManager, AuxiliaryTicketRefusal, BootstrapNonceManager,
    BrowserSessionRole, NonceRefusal, constant_time_eq_32,
};
use super::*;
use fr_core::{ids::RemoteSessionId, time::HostInstant};
use std::{collections::HashSet, net::IpAddr};

const DOMAIN: &[u8] = b"test-only";
fn address() -> IpAddr {
    "100.64.0.1".parse().unwrap()
}
fn time() -> HostInstant {
    HostInstant::from_micros(1_000)
}
fn session() -> RemoteSessionId {
    RemoteSessionId::from_raw(7)
}

#[test]
fn unavailable_and_partial_entropy_never_issue_a_credential() {
    let mut tokens = Tokens::default();
    assert_eq!(tokens.issue_with(DOMAIN, 1, |_| Err(())), Err(()));
    assert!(tokens.key.is_none());
    assert_eq!(tokens.len(), 0);
    let mut calls = 0;
    assert_eq!(
        tokens.issue_with(DOMAIN, 2, |bytes| {
            calls += 1;
            bytes.fill(19);
            if calls == 1 { Ok(()) } else { Err(()) }
        }),
        Err(())
    );
    assert_eq!(calls, 2);
    assert_eq!(tokens.len(), 0);
    assert_eq!(tokens.find(DOMAIN, &[19; 32]), None);
}

#[test]
fn collisions_are_bounded_without_overwriting_a_prior_binding() {
    let mut tokens = Tokens::default();
    let first = tokens
        .issue_with(DOMAIN, 17, |b| {
            b.fill(23);
            Ok(())
        })
        .unwrap();
    let mut calls = 0;
    assert_eq!(
        tokens.issue_with(DOMAIN, 99, |b| {
            calls += 1;
            b.fill(23);
            Ok(())
        }),
        Err(())
    );
    assert_eq!(calls, MAX_CANDIDATES);
    assert_eq!(tokens.len(), 1);
    assert_eq!(
        *tokens.get(tokens.find(DOMAIN, first.as_bytes()).unwrap()),
        17
    );
    let mut calls = 0;
    let next = tokens
        .issue_with(DOMAIN, 29, |b| {
            calls += 1;
            b.fill(if calls == 1 { 23 } else { 42 });
            Ok(())
        })
        .unwrap();
    assert_eq!(calls, 2);
    assert_ne!(first, next);
    assert_eq!(tokens.len(), 2);
}

#[test]
fn storage_contains_domain_separated_keyed_tags_not_bearer_bytes() {
    let mut tokens = Tokens::default();
    let secret = tokens
        .issue_with(DOMAIN, (), |b| {
            b.fill(42);
            Ok(())
        })
        .unwrap();
    assert_ne!(tokens.entries[0].0, *secret.as_bytes());
    assert_eq!(tokens.find(DOMAIN, secret.as_bytes()), Some(0));
    assert_eq!(tokens.find(b"wrong-domain", secret.as_bytes()), None);
    assert_eq!(format!("{tokens:?}"), "Tokens { pending: 1, .. }");
    assert!(!format!("{tokens:?}").contains("42"));
}

#[test]
fn comparison_checks_every_byte_and_both_value_directions() {
    for value in [0, 0x55, 0xaa, 255] {
        let expected = [value; 32];
        assert!(constant_time_eq_32(&expected, &expected));
        for byte in 0..32 {
            let mut different = expected;
            different[byte] ^= 1;
            assert!(!constant_time_eq_32(&expected, &different));
            assert!(!constant_time_eq_32(&different, &expected));
        }
    }
    // Functional regression for the audited primitive; not a hardware timing claim.
}

#[test]
fn wrong_nonce_peer_or_role_does_not_burn_the_legitimate_token() {
    let mut m = BootstrapNonceManager::new();
    let nonce = m
        .issue_nonce(address(), session(), BrowserSessionRole::Controller, time())
        .unwrap();
    assert_eq!(
        m.consume_nonce(
            nonce.as_bytes(),
            "100.64.0.2".parse().unwrap(),
            BrowserSessionRole::Controller,
            time()
        ),
        Err(NonceRefusal::PeerIpMismatch)
    );
    assert_eq!(
        m.consume_nonce(
            nonce.as_bytes(),
            address(),
            BrowserSessionRole::Observer,
            time()
        ),
        Err(NonceRefusal::RoleMismatch)
    );
    assert_eq!(m.pending_count(), 1);
    assert_eq!(
        m.consume_nonce(
            nonce.as_bytes(),
            address(),
            BrowserSessionRole::Controller,
            time()
        ),
        Ok(session())
    );
    assert_eq!(
        m.consume_nonce(
            nonce.as_bytes(),
            address(),
            BrowserSessionRole::Controller,
            time()
        ),
        Err(NonceRefusal::NonceNotFoundOrConsumed)
    );
    assert_eq!(m.pending_count(), 0);
}

#[test]
fn wrong_attachment_binding_does_not_burn_the_legitimate_ticket() {
    let mut m = AuxiliaryTicketManager::new();
    let role = AuxiliaryChannelRole::AudioUplink;
    let ticket = m.issue_ticket(session(), role, address(), time()).unwrap();
    for (peer, id, channel, error) in [
        (
            "100.64.0.2".parse().unwrap(),
            session(),
            role,
            AuxiliaryTicketRefusal::PeerIpMismatch,
        ),
        (
            address(),
            RemoteSessionId::from_raw(8),
            role,
            AuxiliaryTicketRefusal::SessionMismatch,
        ),
        (
            address(),
            session(),
            AuxiliaryChannelRole::AudioDownlink,
            AuxiliaryTicketRefusal::ChannelRoleMismatch,
        ),
    ] {
        assert_eq!(
            m.consume_ticket(Some(ticket.as_bytes()), id, channel, peer, time()),
            Err(error)
        );
        assert_eq!(m.pending_count(), 1);
    }
    assert_eq!(
        m.consume_ticket(Some(ticket.as_bytes()), session(), role, address(), time()),
        Ok(())
    );
    assert_eq!(
        m.consume_ticket(Some(ticket.as_bytes()), session(), role, address(), time()),
        Err(AuxiliaryTicketRefusal::TicketNotFoundOrConsumed)
    );
}

#[test]
fn validity_is_exclusive_and_checked_before_successful_consumption() {
    let (mut nonces, mut tickets) = (BootstrapNonceManager::new(), AuxiliaryTicketManager::new());
    let role = BrowserSessionRole::Observer;
    let channel = AuxiliaryChannelRole::FilesTransfer;
    let n = nonces
        .issue_nonce(address(), session(), role, time())
        .unwrap();
    let t = tickets
        .issue_ticket(session(), channel, address(), time())
        .unwrap();
    let earlier = HostInstant::from_micros(999);
    assert_eq!(
        nonces.consume_nonce(n.as_bytes(), address(), role, earlier),
        Err(NonceRefusal::InvalidClock)
    );
    assert_eq!(
        tickets.consume_ticket(Some(t.as_bytes()), session(), channel, address(), earlier),
        Err(AuxiliaryTicketRefusal::InvalidClock)
    );
    assert_eq!((nonces.pending_count(), tickets.pending_count()), (1, 1));
    let expiry = time()
        .checked_add(super::super::BOOTSTRAP_NONCE_TTL)
        .unwrap();
    assert_eq!(
        nonces.consume_nonce(n.as_bytes(), address(), role, expiry),
        Err(NonceRefusal::NonceExpired)
    );
    assert_eq!(
        tickets.consume_ticket(Some(t.as_bytes()), session(), channel, address(), expiry),
        Err(AuxiliaryTicketRefusal::TicketExpired)
    );
    assert_eq!((nonces.pending_count(), tickets.pending_count()), (0, 0));
    let overflow = HostInstant::from_micros(u64::MAX);
    assert_eq!(
        nonces.issue_nonce(address(), session(), role, overflow),
        Err(NonceRefusal::InvalidClock)
    );
    assert_eq!(
        tickets.issue_ticket(session(), channel, address(), overflow),
        Err(AuxiliaryTicketRefusal::InvalidClock)
    );
}

#[test]
fn public_managers_issue_one_hundred_thousand_distinct_os_tokens() {
    let (mut nonces, mut tickets) = (BootstrapNonceManager::new(), AuxiliaryTicketManager::new());
    let mut issued = HashSet::with_capacity(100_000);
    // Identical public inputs: all unpredictability must come from the OS.
    for _ in 0..50_000 {
        let n = nonces
            .issue_nonce(address(), session(), BrowserSessionRole::Observer, time())
            .unwrap();
        let t = tickets
            .issue_ticket(
                session(),
                AuxiliaryChannelRole::FilesTransfer,
                address(),
                time(),
            )
            .unwrap();
        assert!(issued.insert(*n.as_bytes()));
        assert!(issued.insert(*t.as_bytes()));
        nonces
            .consume_nonce(
                n.as_bytes(),
                address(),
                BrowserSessionRole::Observer,
                time(),
            )
            .unwrap();
        tickets
            .consume_ticket(
                Some(t.as_bytes()),
                session(),
                AuxiliaryChannelRole::FilesTransfer,
                address(),
                time(),
            )
            .unwrap();
    }
    assert_eq!(issued.len(), 100_000);
}
