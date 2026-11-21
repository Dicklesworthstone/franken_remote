//! Portable adapter tests. Desktop availability is synthetic; no socket,
//! browser decoder, installed tailnet, or observing session is qualified here.

use fr_web::adapter::{
    BrowserAdapterError as Error, BrowserAttempt, BrowserConnectionAdapter, MAX_BOOTSTRAP_BYTES,
    MAX_CONTROL_RECORD_BYTES,
};
use fr_wire::{
    input::InputDelivery,
    negotiation::{Role, browser},
    refusal::{self, Reason, Refused},
};

fn bootstrap(role: &str) -> serde_json::Value {
    serde_json::json!({
        "nonce_hex": "a5".repeat(32),
        "expires_in_ms": 15_000,
        "transport_profile": "wss_degraded_tcp_hol_dedicated",
        "profile_id": 3,
        "profile_version": 0,
        "role": role,
        "desktop_available": true,
    })
}

fn awaiting_host() -> (BrowserConnectionAdapter, BrowserAttempt) {
    let mut adapter = BrowserConnectionAdapter::new();
    let attempt = adapter.begin("observer", 1000).unwrap();
    adapter
        .accept_bootstrap(attempt, &bootstrap("observer").to_string(), 2000)
        .unwrap();
    adapter.client_hello(attempt, 3000).unwrap();
    (adapter, attempt)
}

fn refusal(message: Refused, binding: u32) -> Vec<u8> {
    let mut out = [0; 128];
    let length = refusal::encode(
        message,
        binding,
        MAX_CONTROL_RECORD_BYTES,
        &mut out,
        InputDelivery::Reliable,
    )
    .unwrap();
    out[..length].to_vec()
}

fn assert_no_authority(adapter: &BrowserConnectionAdapter) {
    let snapshot = adapter.snapshot();
    assert!(!snapshot.view_fresh);
    assert!(!snapshot.can_request_control);
    assert!(!snapshot.is_controlling);
}

#[test]
fn canonical_hello_uses_the_shared_wire_codec_and_never_opens_a_session() {
    for (role, wire_role) in [
        ("observer", Role::Observe),
        ("controller", Role::RequestControl),
    ] {
        let mut adapter = BrowserConnectionAdapter::new();
        let attempt = adapter.begin(role, 1000).unwrap();
        assert_eq!(attempt.get(), 1);
        let request: serde_json::Value =
            serde_json::from_str(adapter.bootstrap_request(attempt).unwrap()).unwrap();
        assert_eq!(request, serde_json::json!({ "role": role }));
        assert_eq!(adapter.snapshot().state_label, "bootstrapping");
        adapter
            .accept_bootstrap(attempt, &bootstrap(role).to_string(), 2000)
            .unwrap();
        assert_eq!(adapter.snapshot().state_label, "awaiting_transport");
        let bytes = adapter.client_hello(attempt, 3000).unwrap();
        let hello = browser::decode_hello(&bytes, MAX_CONTROL_RECORD_BYTES).unwrap();
        assert_eq!(hello.offer.role, wire_role);
        assert_eq!(hello.offer.versions, [0]);
        assert_eq!(hello.offer.profile, browser::WSS_PROFILE);
        assert_eq!(hello.offer.profile_version, browser::PROFILE_VERSION);
        assert_eq!(hello.nonce(), &[0xa5; 32]);
        assert_eq!(adapter.snapshot().state_label, "awaiting_host");
        assert_no_authority(&adapter);
    }
}

#[test]
fn unavailable_or_missing_capability_retires_without_retaining_nonce_or_peer_text() {
    for availability in [Some(false), None] {
        let mut adapter = BrowserConnectionAdapter::new();
        let attempt = adapter.begin("observer", 1000).unwrap();
        let mut response = serde_json::json!({
            "nonce_hex": "private-bearer-material",
            "refusal_code": "arbitrary-private-peer-text",
        });
        if let Some(available) = availability {
            response["desktop_available"] = available.into();
        }
        assert_eq!(
            adapter.accept_bootstrap(attempt, &response.to_string(), 2000),
            Err(Error::DesktopUnavailable)
        );
        assert_eq!(adapter.snapshot().attempt, None);
        assert_eq!(
            adapter.snapshot().refusal_code,
            Some("browser_session_unavailable")
        );
        assert_eq!(adapter.client_hello(attempt, 3000), Err(Error::Cancelled));
        let snapshot = serde_json::to_string(&adapter.snapshot()).unwrap();
        assert!(!snapshot.contains("private"));
        assert!(!snapshot.contains("nonce"));
        assert_no_authority(&adapter);
    }
}

#[test]
fn bootstrap_checks_profile_role_nonce_limits_and_unknown_authority_fields() {
    let cases = [
        (
            "profile_id",
            serde_json::json!(1),
            Error::UnsupportedProfile,
        ),
        (
            "profile_version",
            serde_json::json!(1),
            Error::UnsupportedProfile,
        ),
        (
            "role",
            serde_json::json!("controller"),
            Error::InvalidBootstrap,
        ),
        (
            "nonce_hex",
            serde_json::json!("0g".repeat(32)),
            Error::InvalidBootstrap,
        ),
        (
            "expires_in_ms",
            serde_json::json!(0),
            Error::InvalidBootstrap,
        ),
        (
            "expires_in_ms",
            serde_json::json!(15_001),
            Error::InvalidBootstrap,
        ),
        (
            "session_id_hex",
            serde_json::json!("ff".repeat(16)),
            Error::InvalidBootstrap,
        ),
        (
            "desktop_available",
            serde_json::json!("true"),
            Error::InvalidBootstrap,
        ),
    ];
    for (field, value, expected) in cases {
        let mut adapter = BrowserConnectionAdapter::new();
        let attempt = adapter.begin("observer", 1000).unwrap();
        let mut response = bootstrap("observer");
        response[field] = value;
        assert_eq!(
            adapter.accept_bootstrap(attempt, &response.to_string(), 2000),
            Err(expected)
        );
        assert_eq!(adapter.snapshot().attempt, None);
        assert_no_authority(&adapter);
    }
    let mut adapter = BrowserConnectionAdapter::new();
    let attempt = adapter.begin("observer", 1000).unwrap();
    assert_eq!(
        adapter.accept_bootstrap(
            attempt,
            r#"{"desktop_available":false,"desktop_available":true}"#,
            2000
        ),
        Err(Error::InvalidBootstrap)
    );
}

#[test]
fn stale_callbacks_cannot_change_replacement_state_even_with_regressed_time() {
    let (mut adapter, first) = awaiting_host();
    let second = adapter.begin("observer", 4000).unwrap();
    assert!(second.get() > first.get());
    let unchanged = adapter.snapshot();
    assert_eq!(
        adapter.accept_bootstrap(first, "invalid", 0),
        Err(Error::Cancelled)
    );
    assert_eq!(adapter.client_hello(first, 0), Err(Error::Cancelled));
    assert_eq!(
        adapter.receive_control(first, b"invalid", 0),
        Err(Error::Cancelled)
    );
    assert_eq!(
        adapter.lifecycle(first, "pagehide", 0),
        Err(Error::Cancelled)
    );
    assert_eq!(adapter.disconnect(first), Err(Error::Cancelled));
    assert_eq!(adapter.snapshot(), unchanged);
}

#[test]
fn bootstrap_expiry_uses_request_start_and_the_deadline_is_exclusive() {
    let mut adapter = BrowserConnectionAdapter::new();
    let attempt = adapter.begin("observer", 1_000_000).unwrap();
    let mut response = bootstrap("observer");
    response["expires_in_ms"] = 2000.into();
    adapter
        .accept_bootstrap(attempt, &response.to_string(), 2_999_999)
        .unwrap();
    adapter.client_hello(attempt, 2_999_999).unwrap();
    assert_eq!(adapter.tick(attempt, 3_000_000), Err(Error::Expired));
    assert_eq!(adapter.snapshot().attempt, None);

    let attempt = adapter.begin("observer", 4_000_000).unwrap();
    assert_eq!(
        adapter.accept_bootstrap(attempt, &response.to_string(), 6_000_000),
        Err(Error::Expired)
    );
}

#[test]
fn progress_does_not_extend_the_overall_handshake_or_allow_time_regression() {
    let (mut adapter, attempt) = awaiting_host();
    adapter.tick(attempt, 9_000_000).unwrap();
    assert_eq!(adapter.tick(attempt, 10_001_000), Err(Error::Expired));
    let attempt = adapter.begin("observer", 11_000_000).unwrap();
    assert_eq!(adapter.tick(attempt, 10_999_999), Err(Error::InvalidClock));
    assert_eq!(
        adapter.snapshot().refusal_code,
        Some("browser_clock_invalid")
    );
}

#[test]
fn hello_is_issued_once_and_cannot_be_sent_before_positive_bootstrap() {
    let (mut adapter, attempt) = awaiting_host();
    assert_eq!(
        adapter.client_hello(attempt, 4000),
        Err(Error::InvalidState)
    );
    assert_eq!(adapter.snapshot().attempt, None);
    let attempt = adapter.begin("observer", 5000).unwrap();
    assert_eq!(
        adapter.client_hello(attempt, 6000),
        Err(Error::InvalidState)
    );
    assert_no_authority(&adapter);
}

#[test]
fn canonical_connection_refusal_preserves_its_typed_reason_and_retires_attempt() {
    let (mut adapter, attempt) = awaiting_host();
    let bytes = refusal(Refused::connection(Reason::RequiredCapability), 0);
    assert_eq!(
        adapter.receive_control(attempt, &bytes, 4000),
        Ok("host_required_capability_missing")
    );
    assert_eq!(adapter.snapshot().state_label, "refused");
    assert_eq!(adapter.snapshot().attempt, None);
    assert_eq!(
        adapter.snapshot().refusal_code,
        Some("host_required_capability_missing")
    );
    assert_no_authority(&adapter);
}

#[test]
fn pre_session_refusals_cannot_invent_operations_effects_or_channel_bindings() {
    let messages = [
        refusal(
            Refused {
                reason: Reason::Expired,
                operation: Some(9),
                stage: None,
            },
            0,
        ),
        refusal(Refused::connection(Reason::ControlUnavailable), 7),
        refusal(Refused::connection(Reason::DirectoryDisabled), 0),
    ];
    for bytes in messages {
        let (mut adapter, attempt) = awaiting_host();
        assert_eq!(
            adapter.receive_control(attempt, &bytes, 4000),
            Err(Error::InvalidControl)
        );
        assert_no_authority(&adapter);
    }
}

#[test]
fn malformed_control_and_oversized_input_are_terminal_bounded_refusals() {
    let valid = refusal(Refused::connection(Reason::RequiredCapability), 0);
    for length in 0..valid.len() {
        let (mut adapter, attempt) = awaiting_host();
        assert_eq!(
            adapter.receive_control(attempt, &valid[..length], 4000),
            Err(Error::InvalidControl)
        );
        assert_eq!(adapter.snapshot().attempt, None);
    }
    let (mut adapter, attempt) = awaiting_host();
    assert_eq!(
        adapter.receive_control(attempt, &vec![0; MAX_CONTROL_RECORD_BYTES + 1], 4000),
        Err(Error::MessageTooLarge)
    );
    let attempt = adapter.begin("observer", 5000).unwrap();
    assert_eq!(
        adapter.accept_bootstrap(attempt, &" ".repeat(MAX_BOOTSTRAP_BYTES + 1), 6000),
        Err(Error::MessageTooLarge)
    );
    assert_no_authority(&adapter);
}

#[test]
fn an_unimplemented_record_never_becomes_session_opened() {
    let mut adapter = BrowserConnectionAdapter::new();
    let attempt = adapter.begin("observer", 1000).unwrap();
    adapter
        .accept_bootstrap(attempt, &bootstrap("observer").to_string(), 2000)
        .unwrap();
    let echoed_hello = adapter.client_hello(attempt, 3000).unwrap();
    assert_eq!(
        adapter.receive_control(attempt, &echoed_hello, 4000),
        Err(Error::UnsupportedControl)
    );
    assert_no_authority(&adapter);
}

#[test]
fn lifecycle_reaches_shared_core_and_pagehide_cannot_be_revived_by_late_work() {
    let (mut adapter, attempt) = awaiting_host();
    adapter.lifecycle(attempt, "hidden", 4000).unwrap();
    adapter.lifecycle(attempt, "focus_lost", 5000).unwrap();
    assert!(!adapter.snapshot().visible);
    assert!(!adapter.snapshot().focused);
    adapter.lifecycle(attempt, "visible", 6000).unwrap();
    adapter.lifecycle(attempt, "focus_gained", 7000).unwrap();
    assert!(adapter.snapshot().visible);
    assert!(adapter.snapshot().focused);
    assert_no_authority(&adapter);
    adapter.lifecycle(attempt, "pagehide", 8000).unwrap();
    assert_eq!(adapter.snapshot().state_label, "closed");
    assert_eq!(adapter.snapshot().attempt, None);
    assert!(!adapter.snapshot().visible);
    assert_eq!(
        adapter.lifecycle(attempt, "visible", 9000),
        Err(Error::Cancelled)
    );
    let replacement = adapter.begin("observer", 10_000).unwrap();
    adapter.lifecycle(replacement, "visible", 11_000).unwrap();
    assert_eq!(
        adapter.lifecycle(attempt, "bfcache_restore", 12_000),
        Err(Error::Cancelled)
    );
    assert_eq!(adapter.snapshot().attempt, Some(replacement.get()));
    assert_no_authority(&adapter);
}
