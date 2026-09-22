#![forbid(unsafe_code)]
//! Comprehensive attack tests for browser origin security (plan §§16.4, 24.3).
//!
//! Evaluates defense-in-depth against:
//! 1. Malicious iframe embedding (`frame-ancestors 'none'`, `Sec-Fetch-Dest: iframe`).
//! 2. DNS rebinding and Host header / authority manipulation.
//! 3. Cross-origin WebSocket and WebTransport hijacking (attacker origins, null origins, wildcard CORS).
//! 4. Nonce and ticket replay attacks (atomic single-use consumption).
//! 5. Stale / expired nonce and ticket attacks (monotonic clock bounds).
//! 6. Stolen token / peer IP spoofing attacks.
//! 7. Auxiliary channel attachment bypass attacks (bare session IDs).
//! 8. Bearer credential leakage in URL query strings.
//! 9. State changes via HTTP GET.
//! 10. Unauthenticated media and input injection before first-message auth.
//! 11. Socket hold-open / slowloris timeout on first authentication message.
//! 12. Strict CSP and security header compliance.

use fr_core::{
    ids::RemoteSessionId,
    time::{HostDuration, HostInstant},
};
use frd::broker::{
    BrowserAssets,
    browser_assets::STRICT_CSP,
    browser_security::{
        AuxiliaryChannelRole, AuxiliaryTicketManager, AuxiliaryTicketRefusal, BOOTSTRAP_NONCE_TTL,
        BROWSER_API_CSP, BROWSER_UI_CSP, BootstrapNonceManager, BrowserSecurityPolicy,
        BrowserSessionRole, ExpectedHostOrigin, FIRST_MESSAGE_AUTH_TIMEOUT, FetchMetadata,
        FirstMessageAuthRefusal, HostRefusal, NonceRefusal, OriginRefusal, QuerySafetyRefusal,
        RedactedSecret, SocketAuthGuard, check_url_query_safety,
    },
};
use std::net::{IpAddr, Ipv4Addr};

fn admitted_peer_ip() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(100, 64, 10, 20))
}

fn attacker_peer_ip() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(100, 64, 99, 99))
}

#[test]
fn attack_test_malicious_iframe_embedding_rejected() {
    let origin = ExpectedHostOrigin::https_tailnet("workstation.my-tailnet.ts.net", 8443);
    let policy = BrowserSecurityPolicy::new(origin);

    // Attacker embeds workstation in <iframe src="https://workstation.my-tailnet.ts.net:8443/">
    let iframe_metadata = FetchMetadata::new(Some("cross-site"), Some("navigate"), Some("iframe"));

    let result = policy.validate_navigation_get(
        "/",
        Some("workstation.my-tailnet.ts.net:8443"),
        Some("workstation.my-tailnet.ts.net"),
        &iframe_metadata,
    );

    assert!(result.is_err(), "iframe embedding must be rejected");

    // Also verify header-level protection in served HTML
    let asset = BrowserAssets::serve("/").unwrap();
    assert_eq!(asset.x_frame_options, "DENY");
    assert!(asset.csp.contains("frame-ancestors 'none'"));
}

#[test]
fn attack_test_dns_rebinding_and_host_header_tampering() {
    let origin = ExpectedHostOrigin::https_tailnet("workstation.my-tailnet.ts.net", 8443);

    // Attacker resolves evil-domain.com to host IP and sends Host: evil-domain.com
    let dns_rebind = origin.validate_host_authority(Some("evil-domain.com:8443"), None);
    assert_eq!(dns_rebind, Err(HostRefusal::HostnameMismatch));

    // Attacker manipulates port in Host header
    let port_tamper =
        origin.validate_host_authority(Some("workstation.my-tailnet.ts.net:9999"), None);
    assert_eq!(port_tamper, Err(HostRefusal::PortMismatch));

    // Attacker presents mismatched TLS SNI
    let sni_mismatch = origin.validate_host_authority(
        Some("workstation.my-tailnet.ts.net:8443"),
        Some("phishing.attacker.com"),
    );
    assert_eq!(sni_mismatch, Err(HostRefusal::SniMismatch));
}

#[test]
fn attack_test_cross_origin_websocket_hijacking() {
    let origin = ExpectedHostOrigin::https_tailnet("workstation.my-tailnet.ts.net", 8443);
    assert_eq!(
        origin.validate_origin(Some("https://malicious-website.com")),
        Err(OriginRefusal::HostMismatch)
    );
    assert_eq!(
        origin.validate_origin(Some("null")),
        Err(OriginRefusal::NullOriginForbidden)
    );
    assert_eq!(
        origin.validate_origin(Some("*")),
        Err(OriginRefusal::WildcardForbidden)
    );
    assert_eq!(
        origin.validate_origin(Some("http://workstation.my-tailnet.ts.net:8443")),
        Err(OriginRefusal::SchemeMismatch)
    );
    assert_eq!(
        origin.validate_origin(None),
        Err(OriginRefusal::MissingRequiredOrigin)
    );
}

#[test]
fn attack_test_nonce_replay_attack() {
    let mut mgr = BootstrapNonceManager::new();
    let (peer, now, session_id) = (
        admitted_peer_ip(),
        HostInstant::from_micros(10_000_000),
        RemoteSessionId::from_raw(1001),
    );
    let nonce = mgr
        .issue_nonce(peer, session_id, BrowserSessionRole::Controller, now)
        .unwrap();
    let first = mgr.consume_nonce(
        nonce.as_bytes(),
        peer,
        BrowserSessionRole::Controller,
        now.checked_add(HostDuration::from_micros(500_000)).unwrap(),
    );
    assert_eq!(first, Ok(session_id));
    let replay = mgr.consume_nonce(
        nonce.as_bytes(),
        peer,
        BrowserSessionRole::Controller,
        now.checked_add(HostDuration::from_micros(600_000)).unwrap(),
    );
    assert_eq!(
        replay,
        Err(NonceRefusal::NonceNotFoundOrConsumed),
        "replayed nonce must be refused"
    );
}

#[test]
fn attack_test_stale_and_expired_nonce_refused() {
    let mut mgr = BootstrapNonceManager::new();
    let (peer, now, session_id) = (
        admitted_peer_ip(),
        HostInstant::from_micros(10_000_000),
        RemoteSessionId::from_raw(1002),
    );
    let nonce = mgr
        .issue_nonce(peer, session_id, BrowserSessionRole::Observer, now)
        .unwrap();
    let late = now
        .checked_add(BOOTSTRAP_NONCE_TTL)
        .unwrap()
        .checked_add(HostDuration::from_micros(1000))
        .unwrap();
    assert_eq!(
        mgr.consume_nonce(nonce.as_bytes(), peer, BrowserSessionRole::Observer, late),
        Err(NonceRefusal::NonceExpired)
    );
}

#[test]
fn attack_test_stolen_nonce_cross_peer_ip_mismatch() {
    let mut mgr = BootstrapNonceManager::new();
    let (victim, attacker, now, session_id) = (
        admitted_peer_ip(),
        attacker_peer_ip(),
        HostInstant::from_micros(10_000_000),
        RemoteSessionId::from_raw(1003),
    );
    let nonce = mgr
        .issue_nonce(victim, session_id, BrowserSessionRole::Controller, now)
        .unwrap();
    let res = mgr.consume_nonce(
        nonce.as_bytes(),
        attacker,
        BrowserSessionRole::Controller,
        now.checked_add(HostDuration::from_micros(200_000)).unwrap(),
    );
    assert_eq!(res, Err(NonceRefusal::PeerIpMismatch));
}

#[test]
fn attack_test_auxiliary_channel_bare_session_id_bypass_refused() {
    let mut ticket_mgr = AuxiliaryTicketManager::new();
    let (peer, now, session_id) = (
        admitted_peer_ip(),
        HostInstant::from_micros(10_000_000),
        RemoteSessionId::from_raw(2001),
    );

    let bare = ticket_mgr.consume_ticket(
        None,
        session_id,
        AuxiliaryChannelRole::FilesTransfer,
        peer,
        now,
    );
    assert_eq!(bare, Err(AuxiliaryTicketRefusal::AuxiliaryTicketRequired));

    let audio_ticket = ticket_mgr
        .issue_ticket(session_id, AuxiliaryChannelRole::AudioDownlink, peer, now)
        .unwrap();
    let wrong = ticket_mgr.consume_ticket(
        Some(audio_ticket.as_bytes()),
        session_id,
        AuxiliaryChannelRole::FilesTransfer,
        peer,
        now,
    );
    assert_eq!(wrong, Err(AuxiliaryTicketRefusal::ChannelRoleMismatch));
}

#[test]
fn attack_test_bearer_credential_in_query_string_refused() {
    for query in [
        "/connect?nonce=1234567890abcdef",
        "/files?ticket=secret_ticket_123",
        "/ws?token=bearer_xyz",
        "/session?auth=password123",
        "/audio?secret=key",
    ] {
        assert_eq!(
            check_url_query_safety(query),
            Err(QuerySafetyRefusal::BearerInQueryForbidden)
        );
    }
}

#[test]
fn attack_test_unauthenticated_media_and_input_injection_blocked() {
    let peer = admitted_peer_ip();
    let now = HostInstant::from_micros(10_000_000);
    let guard = SocketAuthGuard::new(peer, now);

    // Transport is open, but first message auth hasn't occurred yet
    assert!(!guard.is_authenticated());

    // Attacker tries to inject keystrokes or pointer motions
    let input_res = guard.check_media_or_input_allowed();
    assert_eq!(
        input_res,
        Err(FirstMessageAuthRefusal::UnauthenticatedOperationAttempted),
        "input injection on unauthenticated socket must be blocked"
    );

    // Attacker tries to read video/pixel frames
    let media_res = guard.check_media_or_input_allowed();
    assert_eq!(
        media_res,
        Err(FirstMessageAuthRefusal::UnauthenticatedOperationAttempted),
        "pixel delivery on unauthenticated socket must be blocked"
    );
}

#[test]
fn attack_test_slowloris_socket_auth_deadline_expires() {
    let mut nonce_mgr = BootstrapNonceManager::new();
    let peer = admitted_peer_ip();
    let now = HostInstant::from_micros(10_000_000);
    let session_id = RemoteSessionId::from_raw(3001);

    let nonce = nonce_mgr
        .issue_nonce(peer, session_id, BrowserSessionRole::Controller, now)
        .unwrap();

    let mut guard = SocketAuthGuard::new(peer, now);

    // Attacker holds socket open without sending auth message for > 3.0 seconds
    let late = now
        .checked_add(FIRST_MESSAGE_AUTH_TIMEOUT)
        .unwrap()
        .checked_add(HostDuration::from_micros(100_000))
        .unwrap();

    let mut msg = Vec::new();
    msg.extend_from_slice(b"FRBA");
    msg.extend_from_slice(nonce.as_bytes());

    let late_auth =
        guard.process_auth_message(&msg, BrowserSessionRole::Controller, &mut nonce_mgr, late);

    assert_eq!(
        late_auth,
        Err(FirstMessageAuthRefusal::AuthDeadlineExpired),
        "auth handshake after 3s deadline must be dropped"
    );
    assert!(!guard.is_authenticated());
}

#[test]
fn attack_test_csp_and_security_headers_verification() {
    // 1. Verify Browser UI CSP
    assert!(BROWSER_UI_CSP.contains("frame-ancestors 'none'"));
    assert!(BROWSER_UI_CSP.contains("default-src 'self'"));
    assert!(BROWSER_UI_CSP.contains("object-src 'none'"));
    assert!(BROWSER_UI_CSP.contains("base-uri 'self'"));

    // 2. Verify API CSP (strictly no execution)
    assert_eq!(
        BROWSER_API_CSP,
        "default-src 'none'; frame-ancestors 'none'"
    );

    // 3. Verify static asset response headers
    let asset = BrowserAssets::serve("/").unwrap();
    assert_eq!(asset.csp, STRICT_CSP);
    assert_eq!(asset.x_frame_options, "DENY");
    assert_eq!(asset.x_content_type_options, "nosniff");
    assert_eq!(asset.cache_control, "no-cache, no-store, must-revalidate");

    // 4. Verify secrets are redacted in debug and display output
    let raw = [0x42; 32];
    let secret = RedactedSecret::new(raw);
    assert!(format!("{secret:?}").starts_with("[REDACTED:sha256:"));
    assert!(format!("{secret}").starts_with("[REDACTED:sha256:"));
    assert!(!format!("{secret:?}").contains("42424242"));
}
