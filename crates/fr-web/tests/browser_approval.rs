//! The real Rust browser owner with canonical admitted-host records. Native
//! prompt, network and browser execution remain separate qualification layers.
use fr_core::{
    ids::{HostBootId, OsSessionId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_web::adapter::{BrowserAdapterError as Error, BrowserAttempt, BrowserConnectionAdapter};
use fr_wire::{
    input::InputDelivery,
    negotiation::{ControlBinding, Message, Offer, Role, Selection, browser},
    refusal::{self, Reason, Refused},
};

const START: u64 = 1_000_000;
const REQUEST: u128 = 13;

fn bootstrap(lifetime_ms: u32) -> String {
    serde_json::json!({
        "nonce_hex": "a5".repeat(32), "expires_in_ms": lifetime_ms,
        "transport_profile": "wss_degraded_tcp_hol_dedicated",
        "profile_id": 3, "profile_version": 0, "role": "observer", "desktop_available": true,
    })
    .to_string()
}
fn record(message: Message) -> Vec<u8> {
    let mut bytes = [0; browser::MAX_RECORD];
    let n = browser::encode(&message, bytes.len(), &mut bytes).unwrap();
    bytes[..n].to_vec()
}
fn offer(required: Option<bool>) -> Offer {
    let mut host = browser::offer(ProtocolLimits::ABSOLUTE);
    if let Some(required) = required {
        host.capabilities
            .iter_mut()
            .find(|capability| capability.name == browser::APPROVAL_CAPABILITY)
            .unwrap()
            .required = required;
    } else {
        host.capabilities
            .retain(|capability| capability.name != browser::APPROVAL_CAPABILITY);
    }
    host
}
fn selected(required: Option<bool>) -> (BrowserConnectionAdapter, BrowserAttempt, Selection) {
    let mut adapter = BrowserConnectionAdapter::new();
    let attempt = adapter.begin("observer", START).unwrap();
    adapter
        .accept_bootstrap(attempt, &bootstrap(15_000), START + 1)
        .unwrap();
    adapter.client_hello(attempt, START + 2).unwrap();
    adapter
        .receive_control(
            attempt,
            &record(Message::HostCapabilities(offer(required))),
            START + 3,
        )
        .unwrap();
    let bytes = adapter.take_control(attempt, START + 4).unwrap().unwrap();
    let Message::SelectedConfiguration(selection) =
        browser::decode(&bytes, browser::MAX_RECORD, 0).unwrap()
    else {
        panic!("selected configuration required")
    };
    adapter.control_queued(attempt, START + 5).unwrap();
    (adapter, attempt, selection)
}
fn notice(request: u128, deadline_us: u64, role: Role) -> Vec<u8> {
    record(Message::ApprovalRequired {
        request: RemoteSessionId::from_raw(request),
        deadline_us,
        role,
    })
}
fn opened(selection: Selection, request: u128) -> Vec<u8> {
    record(Message::SessionOpened {
        binding: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(request),
        },
        selection,
        observation_until_us: 1,
    })
}
fn no_authority(adapter: &BrowserConnectionAdapter) {
    let snapshot = adapter.snapshot();
    assert!(!snapshot.view_fresh);
    assert!(!snapshot.can_request_control);
    assert!(!snapshot.is_controlling);
    assert!(!adapter.needs_video());
    assert_eq!(adapter.desktop_snapshot().retained_decoders, 0);
    assert_eq!(adapter.desktop_snapshot().inflight_frames, 0);
}

#[test]
fn approval_uses_original_thirty_second_ceiling_without_inventing_observation() {
    let (mut adapter, attempt, selection) = selected(Some(true));
    // Host and client clocks are unrelated: a host timestamp smaller than the
    // local clock remains a notice, never an interpreted client expiry.
    adapter
        .receive_control(
            attempt,
            &notice(REQUEST, 1, Role::Observe),
            START + 9_999_999,
        )
        .unwrap();
    assert_eq!(adapter.snapshot().state_label, "awaiting_approval");
    no_authority(&adapter);
    adapter.tick(attempt, START + 29_000_000).unwrap();
    no_authority(&adapter);
    adapter
        .receive_control(attempt, &opened(selection, REQUEST), START + 29_000_001)
        .unwrap();
    assert_eq!(adapter.snapshot().state_label, "awaiting_media");
    no_authority(&adapter);
    // Consent does not start a fresh media-startup budget either.
    assert_eq!(
        adapter.tick(attempt, START + browser::APPROVAL_STARTUP_BUDGET_US),
        Err(Error::Expired)
    );
}

#[test]
fn pending_approval_expires_at_original_start_not_notice_arrival() {
    let (mut adapter, attempt, _) = selected(Some(true));
    adapter
        .receive_control(
            attempt,
            &notice(REQUEST, u64::MAX, Role::Observe),
            START + 9_000_000,
        )
        .unwrap();
    adapter
        .tick(attempt, START + browser::APPROVAL_STARTUP_BUDGET_US - 1)
        .unwrap();
    assert_eq!(
        adapter.tick(attempt, START + browser::APPROVAL_STARTUP_BUDGET_US),
        Err(Error::ApprovalExpired)
    );
    assert_eq!(
        adapter.snapshot().refusal_code,
        Some("browser_approval_expired")
    );
    assert_eq!(adapter.snapshot().attempt, None);
    no_authority(&adapter);
}

#[test]
fn a_late_notice_cannot_rescue_expired_negotiation() {
    let (mut adapter, attempt, _) = selected(Some(true));
    assert_eq!(
        adapter.receive_control(
            attempt,
            &notice(REQUEST, 50_000_000, Role::Observe),
            START + 10_000_000
        ),
        Err(Error::Expired)
    );
    assert_eq!(adapter.snapshot().attempt, None);
    no_authority(&adapter);
}

#[test]
fn absent_capability_or_required_notice_cannot_be_bypassed() {
    let (mut adapter, attempt, _) = selected(None);
    assert_eq!(
        adapter.receive_control(
            attempt,
            &notice(REQUEST, 50_000_000, Role::Observe),
            START + 6
        ),
        Err(Error::UnsupportedControl)
    );
    no_authority(&adapter);
    let (mut adapter, attempt, selection) = selected(Some(true));
    assert_eq!(
        adapter.receive_control(attempt, &opened(selection, REQUEST), START + 6),
        Err(Error::UnsupportedControl)
    );
    no_authority(&adapter);
    // Optional implementation support alone does not require a prompt.
    let (mut adapter, attempt, selection) = selected(Some(false));
    adapter
        .receive_control(attempt, &opened(selection, REQUEST), START + 6)
        .unwrap();
    assert_eq!(adapter.snapshot().state_label, "awaiting_media");
    assert_eq!(
        adapter.tick(attempt, START + 10_000_000),
        Err(Error::Expired)
    );
}

#[test]
fn notice_is_single_use_and_open_names_exact_original_request() {
    for (request, deadline, role) in [
        (REQUEST, 50_000_000, Role::Observe),
        (REQUEST + 1, 50_000_000, Role::Observe),
        (REQUEST, 60_000_000, Role::Observe),
        (REQUEST, 50_000_000, Role::RequestControl),
    ] {
        let (mut adapter, attempt, _) = selected(Some(true));
        adapter
            .receive_control(
                attempt,
                &notice(REQUEST, 50_000_000, Role::Observe),
                START + 6,
            )
            .unwrap();
        assert_eq!(
            adapter.receive_control(attempt, &notice(request, deadline, role), START + 7),
            Err(Error::UnsupportedControl)
        );
        no_authority(&adapter);
    }
    let (mut adapter, attempt, selection) = selected(Some(true));
    adapter
        .receive_control(
            attempt,
            &notice(REQUEST, 50_000_000, Role::Observe),
            START + 6,
        )
        .unwrap();
    assert_eq!(
        adapter.receive_control(attempt, &opened(selection, REQUEST + 1), START + 7),
        Err(Error::UnsupportedControl)
    );
    no_authority(&adapter);
}

#[test]
fn invalid_notices_and_unqueued_selection_never_enter_approval() {
    let valid = notice(REQUEST, 50_000_000, Role::Observe);
    for bytes in [
        notice(0, 50_000_000, Role::Observe),
        notice(REQUEST, 50_000_000, Role::RequestControl),
        {
            let mut bytes = valid.clone();
            bytes[40..48].fill(0);
            bytes
        },
    ] {
        let (mut adapter, attempt, _) = selected(Some(true));
        assert!(adapter.receive_control(attempt, &bytes, START + 6).is_err());
        no_authority(&adapter);
    }
    for take in [false, true] {
        let mut adapter = BrowserConnectionAdapter::new();
        let attempt = adapter.begin("observer", START).unwrap();
        adapter
            .accept_bootstrap(attempt, &bootstrap(15_000), START + 1)
            .unwrap();
        adapter.client_hello(attempt, START + 2).unwrap();
        adapter
            .receive_control(
                attempt,
                &record(Message::HostCapabilities(offer(Some(true)))),
                START + 3,
            )
            .unwrap();
        if take {
            adapter.take_control(attempt, START + 4).unwrap();
        }
        assert_eq!(
            adapter.receive_control(attempt, &valid, START + 5),
            Err(Error::UnsupportedControl)
        );
        no_authority(&adapter);
    }
}

#[test]
fn cancellation_and_denial_fence_original_consent_without_poisoning_a_replacement() {
    for reason in [Reason::LocalApprovalDenied, Reason::ApprovalExpired] {
        let (mut adapter, attempt, _) = selected(Some(true));
        adapter
            .receive_control(
                attempt,
                &notice(REQUEST, 50_000_000, Role::Observe),
                START + 6,
            )
            .unwrap();
        let mut bytes = [0; 128];
        let n = refusal::encode(
            Refused::connection(reason),
            0,
            browser::MAX_RECORD,
            &mut bytes,
            InputDelivery::Reliable,
        )
        .unwrap();
        let code = adapter
            .receive_control(attempt, &bytes[..n], START + 7)
            .unwrap();
        assert_eq!(
            code,
            if reason == Reason::LocalApprovalDenied {
                "host_local_approval_denied"
            } else {
                "host_approval_expired"
            }
        );
        no_authority(&adapter);
    }
    for event in ["pagehide", "bfcache_restore"] {
        let (mut adapter, first, selection) = selected(Some(true));
        adapter
            .receive_control(
                first,
                &notice(REQUEST, 50_000_000, Role::Observe),
                START + 6,
            )
            .unwrap();
        adapter.lifecycle(first, event, START + 7).unwrap();
        let replacement = adapter.begin("observer", START + 8).unwrap();
        let before = adapter.snapshot();
        assert_eq!(
            adapter.receive_control(first, &opened(selection, REQUEST), START + 9),
            Err(Error::Cancelled)
        );
        assert_eq!(adapter.snapshot(), before);
        assert_eq!(adapter.snapshot().attempt, Some(replacement.get()));
        assert_eq!(
            adapter.tick(replacement, START + 10_000_008),
            Err(Error::Expired)
        );
    }
}

#[test]
fn original_nonce_expiry_still_bounds_hello_and_cannot_be_rescued_by_consent() {
    let mut adapter = BrowserConnectionAdapter::new();
    let attempt = adapter.begin("observer", START).unwrap();
    adapter
        .accept_bootstrap(attempt, &bootstrap(1000), START + 1)
        .unwrap();
    assert_eq!(
        adapter.client_hello(attempt, START + 1_000_000),
        Err(Error::Expired)
    );
    assert_eq!(
        adapter.receive_control(
            attempt,
            &notice(REQUEST, 50_000_000, Role::Observe),
            START + 1_000_001
        ),
        Err(Error::Cancelled)
    );
    no_authority(&adapter);
}
