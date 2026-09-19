#![cfg(target_os = "linux")]

use fr_core::ids::{
    DisplayGeometryGeneration, HostBootId, InputLeaseId, OsSessionId, RemoteSessionId,
};
use frd::broker::{
    AssetError, BrokerService, BrokerStateInspect, BrowserAssets, DaemonConfig, IdleHarness,
    IpcError, IpcHeader, IpcMessage, IpcMessageKind, MAX_IDLE_CPU_PERCENT, MAX_IDLE_RSS_BYTES,
    MAX_IPC_PAYLOAD_BYTES, PeerIdentity, PeerIdentityCache, ProcessGeneration, ProcessRole,
    RegistryError, RoleCapability, STRICT_CSP, SocketCredentials, verify_incoming_message,
};
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

// -----------------------------------------------------------------------------
// 1. Process Role and Capability Boundaries
// -----------------------------------------------------------------------------

#[test]
fn media_worker_strictly_denied_privileged_capabilities() {
    let worker = ProcessRole::MediaWorker;

    // Security invariant: workers receive NO Tailscale socket, NO cert keys, NO approval, NO input lease
    assert!(!worker.allows_capability(RoleCapability::TailscaleControlSocket));
    assert!(!worker.allows_capability(RoleCapability::CertificateKeys));
    assert!(!worker.allows_capability(RoleCapability::ApprovalEndpoint));
    assert!(!worker.allows_capability(RoleCapability::InputLease));
    assert!(!worker.allows_capability(RoleCapability::ClipboardAccess));
    assert!(!worker.allows_capability(RoleCapability::AuditLog));

    // Workers only retain media capture and GPU surface capabilities
    assert!(worker.allows_capability(RoleCapability::MediaCapture));
    assert!(worker.allows_capability(RoleCapability::GpuSurfaces));
}

#[test]
fn session_agent_retains_consent_and_input_only() {
    let agent = ProcessRole::SessionAgent;

    assert!(agent.allows_capability(RoleCapability::ApprovalEndpoint));
    assert!(agent.allows_capability(RoleCapability::InputLease));
    assert!(agent.allows_capability(RoleCapability::DisplayQuery));
    assert!(agent.allows_capability(RoleCapability::ClipboardAccess));

    assert!(!agent.allows_capability(RoleCapability::TailscaleControlSocket));
    assert!(!agent.allows_capability(RoleCapability::CertificateKeys));
    assert!(!agent.allows_capability(RoleCapability::MediaCapture));
    assert!(!agent.allows_capability(RoleCapability::GpuSurfaces));
}

#[test]
fn broker_has_listeners_and_certs_without_input_or_gpu() {
    let broker = ProcessRole::Broker;

    assert!(broker.allows_capability(RoleCapability::TailscaleControlSocket));
    assert!(broker.allows_capability(RoleCapability::CertificateKeys));
    assert!(broker.allows_capability(RoleCapability::DisplayQuery));
    assert!(broker.allows_capability(RoleCapability::AuditLog));

    // Broker has no direct input authority or media/GPU allocations
    assert!(!broker.allows_capability(RoleCapability::InputLease));
    assert!(!broker.allows_capability(RoleCapability::ApprovalEndpoint));
    assert!(!broker.allows_capability(RoleCapability::MediaCapture));
    assert!(!broker.allows_capability(RoleCapability::GpuSurfaces));
}

// -----------------------------------------------------------------------------
// 2. Local IPC Forgery and Boundary Enforcement
// -----------------------------------------------------------------------------

const AUTHORIZED_UID: u32 = 1000;
const PEER_PID: u32 = 5555;
const PEER_PID_I32: i32 = 5555;

fn sample_header(role: ProcessRole, generation: u64, uid: u32, kind: IpcMessageKind) -> IpcHeader {
    IpcHeader::new(
        role,
        ProcessGeneration::from_raw(generation),
        uid,
        PEER_PID,
        kind,
        0,
    )
}

#[test]
fn ipc_forgery_refuses_wrong_role() {
    let header = sample_header(
        ProcessRole::MediaWorker,
        1,
        AUTHORIZED_UID,
        IpcMessageKind::ConsentDecision,
    );
    let cred = SocketCredentials::new(Some(PEER_PID_I32), AUTHORIZED_UID, AUTHORIZED_UID);

    let res = verify_incoming_message(
        &header,
        Some(&cred),
        ProcessRole::SessionAgent, // Expecting SessionAgent on this channel
        AUTHORIZED_UID,
        ProcessGeneration::INITIAL,
    );

    assert_eq!(
        res,
        Err(IpcError::WrongRole {
            expected: ProcessRole::SessionAgent,
            actual: ProcessRole::MediaWorker,
        })
    );
}

#[test]
fn ipc_forgery_refuses_stale_generation() {
    let header = sample_header(
        ProcessRole::SessionAgent,
        1, // Stale generation
        AUTHORIZED_UID,
        IpcMessageKind::InputLeaseRequest,
    );
    let cred = SocketCredentials::new(Some(PEER_PID_I32), AUTHORIZED_UID, AUTHORIZED_UID);

    let res = verify_incoming_message(
        &header,
        Some(&cred),
        ProcessRole::SessionAgent,
        AUTHORIZED_UID,
        ProcessGeneration::from_raw(2), // Active generation is 2
    );

    assert_eq!(
        res,
        Err(IpcError::StaleProcessGeneration {
            expected: 2,
            actual: 1,
        })
    );
}

#[test]
fn ipc_forgery_refuses_unauthorized_user() {
    let rogue_uid = 1002;
    let header = sample_header(
        ProcessRole::SessionAgent,
        1,
        rogue_uid,
        IpcMessageKind::InputLeaseRequest,
    );
    let cred = SocketCredentials::new(Some(PEER_PID_I32), rogue_uid, rogue_uid);

    let res = verify_incoming_message(
        &header,
        Some(&cred),
        ProcessRole::SessionAgent,
        AUTHORIZED_UID, // Expected 1000, got 1002
        ProcessGeneration::INITIAL,
    );

    assert_eq!(
        res,
        Err(IpcError::UnauthorizedUser {
            expected_uid: AUTHORIZED_UID,
            actual_uid: rogue_uid,
        })
    );
}

#[test]
fn ipc_forgery_refuses_forged_socket_credentials() {
    // Header claims AUTHORIZED_UID, but kernel socket credentials show attacker UID
    let header = sample_header(
        ProcessRole::SessionAgent,
        1,
        AUTHORIZED_UID,
        IpcMessageKind::InputLeaseRequest,
    );
    let cred = SocketCredentials::new(Some(PEER_PID_I32), 1337, 1337);

    let res = verify_incoming_message(
        &header,
        Some(&cred),
        ProcessRole::SessionAgent,
        AUTHORIZED_UID,
        ProcessGeneration::INITIAL,
    );

    assert_eq!(
        res,
        Err(IpcError::ForgedCredentials {
            declared_uid: AUTHORIZED_UID,
            socket_uid: 1337,
        })
    );
}

#[test]
fn ipc_forgery_refuses_worker_requesting_input_lease() {
    let header = sample_header(
        ProcessRole::MediaWorker,
        1,
        AUTHORIZED_UID,
        IpcMessageKind::InputLeaseRequest,
    );
    let cred = SocketCredentials::new(Some(PEER_PID_I32), AUTHORIZED_UID, AUTHORIZED_UID);

    let res = verify_incoming_message(
        &header,
        Some(&cred),
        ProcessRole::MediaWorker,
        AUTHORIZED_UID,
        ProcessGeneration::INITIAL,
    );

    assert_eq!(
        res,
        Err(IpcError::ForbiddenCapability {
            role: ProcessRole::MediaWorker,
            capability: RoleCapability::InputLease,
        })
    );
}

#[test]
fn ipc_forgery_refuses_worker_requesting_tls_certificates() {
    let header = sample_header(
        ProcessRole::MediaWorker,
        1,
        AUTHORIZED_UID,
        IpcMessageKind::TailscaleCertRequest,
    );
    let cred = SocketCredentials::new(Some(PEER_PID_I32), AUTHORIZED_UID, AUTHORIZED_UID);

    let res = verify_incoming_message(
        &header,
        Some(&cred),
        ProcessRole::MediaWorker,
        AUTHORIZED_UID,
        ProcessGeneration::INITIAL,
    );

    assert_eq!(
        res,
        Err(IpcError::ForbiddenCapability {
            role: ProcessRole::MediaWorker,
            capability: RoleCapability::CertificateKeys,
        })
    );
}

#[test]
fn ipc_forgery_refuses_worker_requesting_tailscale_status() {
    let header = sample_header(
        ProcessRole::MediaWorker,
        1,
        AUTHORIZED_UID,
        IpcMessageKind::TailscaleStatusQuery,
    );
    let cred = SocketCredentials::new(Some(PEER_PID_I32), AUTHORIZED_UID, AUTHORIZED_UID);

    let res = verify_incoming_message(
        &header,
        Some(&cred),
        ProcessRole::MediaWorker,
        AUTHORIZED_UID,
        ProcessGeneration::INITIAL,
    );

    assert_eq!(
        res,
        Err(IpcError::ForbiddenCapability {
            role: ProcessRole::MediaWorker,
            capability: RoleCapability::TailscaleControlSocket,
        })
    );
}

#[test]
fn ipc_forgery_refuses_worker_sending_consent_decision() {
    let header = sample_header(
        ProcessRole::MediaWorker,
        1,
        AUTHORIZED_UID,
        IpcMessageKind::ConsentDecision,
    );
    let cred = SocketCredentials::new(Some(PEER_PID_I32), AUTHORIZED_UID, AUTHORIZED_UID);

    let res = verify_incoming_message(
        &header,
        Some(&cred),
        ProcessRole::MediaWorker,
        AUTHORIZED_UID,
        ProcessGeneration::INITIAL,
    );

    assert_eq!(
        res,
        Err(IpcError::ForbiddenCapability {
            role: ProcessRole::MediaWorker,
            capability: RoleCapability::ApprovalEndpoint,
        })
    );
}

#[test]
fn ipc_message_roundtrip_and_oversize_refusal() {
    let header = sample_header(
        ProcessRole::SessionAgent,
        1,
        AUTHORIZED_UID,
        IpcMessageKind::Ping,
    );
    let payload = b"hello-frd-broker".to_vec();
    let msg = IpcMessage::new(header, payload.clone()).unwrap();

    let encoded = msg.encode();
    let decoded = IpcMessage::decode(&encoded).unwrap();
    assert_eq!(decoded.header, msg.header);
    assert_eq!(decoded.payload, payload);

    let oversize_payload = vec![0u8; MAX_IPC_PAYLOAD_BYTES + 1];
    assert_eq!(
        IpcMessage::new(header, oversize_payload),
        Err(IpcError::PayloadTooLarge {
            length: MAX_IPC_PAYLOAD_BYTES + 1,
            maximum: MAX_IPC_PAYLOAD_BYTES,
        })
    );
}

// -----------------------------------------------------------------------------
// 3. Peer Identity Cache
// -----------------------------------------------------------------------------

#[test]
fn peer_cache_enforces_capacity_and_ttl_pruning() {
    let mut cache = PeerIdentityCache::new(2, Duration::from_secs(10));
    let ip1 = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
    let ip2 = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 2));
    let ip3 = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 3));

    let p1 = PeerIdentity {
        ip: ip1,
        node_id: "node:1".into(),
        fqdn: "n1.ts.net".into(),
        display_name: "N1".into(),
        login_name: "a@ts.net".into(),
        is_own_user: true,
        tags: vec![],
        cached_at_us: 1_000_000,
        valid_until_us: 5_000_000, // expires at 5s
    };
    let p2 = PeerIdentity {
        ip: ip2,
        node_id: "node:2".into(),
        fqdn: "n2.ts.net".into(),
        display_name: "N2".into(),
        login_name: "a@ts.net".into(),
        is_own_user: true,
        tags: vec![],
        cached_at_us: 1_000_000,
        valid_until_us: 20_000_000,
    };
    let p3 = PeerIdentity {
        ip: ip3,
        node_id: "node:3".into(),
        fqdn: "n3.ts.net".into(),
        display_name: "N3".into(),
        login_name: "a@ts.net".into(),
        is_own_user: true,
        tags: vec![],
        cached_at_us: 1_000_000,
        valid_until_us: 25_000_000,
    };

    cache.insert(p1, 1_000_000).unwrap();
    cache.insert(p2, 1_000_000).unwrap();
    assert_eq!(cache.len(), 2);

    // At t=6s, p1 is expired. Inserting p3 evicts expired p1.
    cache.insert(p3, 6_000_000).unwrap();
    assert_eq!(cache.len(), 2);
    assert_eq!(cache.lookup(&ip1, 6_000_000), None);
    assert!(cache.lookup(&ip2, 6_000_000).is_some());
    assert!(cache.lookup(&ip3, 6_000_000).is_some());
}

// -----------------------------------------------------------------------------
// 4. Session Registry & Single Controller Invariant
// -----------------------------------------------------------------------------

#[test]
fn session_registry_enforces_single_controller_and_generation_fencing() {
    let mut reg = frd::broker::SessionRegistry::new(
        HostBootId::from_raw(1),
        OsSessionId::from_raw(1),
        2, // max 2 viewers
    );

    let ip = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
    let s1 = RemoteSessionId::from_raw(101);
    let s2 = RemoteSessionId::from_raw(102);
    let s3 = RemoteSessionId::from_raw(103);

    // Admit 2 viewers
    reg.admit_viewer(s1, ip, "alice@ts.net".into(), true, 1000)
        .unwrap();
    reg.admit_viewer(s2, ip, "bob@ts.net".into(), true, 1000)
        .unwrap();

    // 3rd viewer rejected (capacity)
    assert_eq!(
        reg.admit_viewer(s3, ip, "charlie@ts.net".into(), true, 1000),
        Err(RegistryError::CapacityExceeded {
            current: 2,
            maximum: 2,
        })
    );

    // s1 acquires input lease
    let l1 = InputLeaseId::from_raw(201);
    reg.grant_input_lease(s1, l1, 5000, 1000).unwrap();
    assert_eq!(reg.active_controller(), Some(s1));

    // s2 attempts to acquire input lease while s1 is active -> refused
    let l2 = InputLeaseId::from_raw(202);
    assert_eq!(
        reg.grant_input_lease(s2, l2, 5000, 2000),
        Err(RegistryError::ControllerAlreadyActive {
            active_session_id: s1,
        })
    );

    // Geometry generation change fences input
    let geo = DisplayGeometryGeneration::INITIAL;
    assert!(reg.validate_input_submission(s1, l1, geo, 2000).is_ok());

    let next_geo = reg.advance_geometry_generation().unwrap();
    // Old geometry generation is refused
    assert!(matches!(
        reg.validate_input_submission(s1, l1, geo, 2000),
        Err(RegistryError::StaleGeneration { .. })
    ));
    // New geometry generation is accepted
    assert!(
        reg.validate_input_submission(s1, l1, next_geo, 2000)
            .is_ok()
    );

    // Teardown cleans up and advances generation
    let plan = reg.teardown_all();
    assert_eq!(plan.sessions_closed, 2);
    assert!(plan.controller_revoked);
    assert_eq!(reg.session_count(), 0);
    assert_eq!(reg.active_controller(), None);
}

// -----------------------------------------------------------------------------
// 5. Browser Assets & Security Headers
// -----------------------------------------------------------------------------

#[test]
fn browser_assets_serve_strict_headers_and_block_traversal() {
    let index = BrowserAssets::serve("/index.html").unwrap();
    assert_eq!(index.status_code, 200);
    assert_eq!(index.csp, STRICT_CSP);
    assert_eq!(index.x_frame_options, "DENY");
    assert_eq!(index.x_content_type_options, "nosniff");

    // Directory traversal attacks are refused
    assert_eq!(
        BrowserAssets::serve("/../etc/shadow"),
        Err(AssetError::PathTraversalForbidden)
    );
    assert_eq!(
        BrowserAssets::serve("/assets/../../etc/passwd"),
        Err(AssetError::PathTraversalForbidden)
    );
    assert_eq!(
        BrowserAssets::serve("/foo\\bar"),
        Err(AssetError::InvalidCharacters)
    );
    assert_eq!(
        BrowserAssets::serve("/foo\0bar"),
        Err(AssetError::InvalidCharacters)
    );
}

// -----------------------------------------------------------------------------
// 6. Idle Measurement Harness & Operating Envelope
// -----------------------------------------------------------------------------

#[test]
fn idle_broker_conforms_to_operating_envelope() {
    let config = DaemonConfig::default();
    let ip = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
    let service = BrokerService::new(
        config,
        HostBootId::from_raw(1),
        OsSessionId::from_raw(1),
        "desktop.example.ts.net".into(),
        vec![ip],
    );

    // Idle daemon invariant: 0 captures, 0 encoders, 0 GPU surfaces
    assert!(service.is_idle());
    assert_eq!(service.active_captures(), 0);
    assert_eq!(service.active_encoders(), 0);
    assert_eq!(service.gpu_surfaces(), 0);

    // Run idle quiet interval measurement
    let report = IdleHarness::measure(Duration::from_millis(50), &service);

    // Check envelope constraints
    assert!(
        report.rss_bytes <= MAX_IDLE_RSS_BYTES,
        "Idle RSS {} exceeds 25 MiB ceiling {}",
        report.rss_bytes,
        MAX_IDLE_RSS_BYTES
    );
    assert!(
        report.cpu_percent <= MAX_IDLE_CPU_PERCENT,
        "Idle CPU {:.4}% exceeds 0.1% ceiling",
        report.cpu_percent
    );
    assert_eq!(report.active_captures, 0);
    assert_eq!(report.active_encoders, 0);
    assert_eq!(report.gpu_surfaces, 0);
    assert!(report.passed, "Idle report did not pass envelope check");
    assert!(report.verify_envelope().is_ok());
}
