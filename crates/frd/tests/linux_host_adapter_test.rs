//! Integration and fault tests for Linux host adapter (bead fr-p2-host-linux-4e4, plan §10.1, §23 Phase 2, §24.2).
//!
//! Tests:
//! 1. Portal sequence & ordering: clipboard must be configured before Start.
//! 2. Authorization by returned grants: ungranted device categories rejected at injection time.
//! 3. Single-use restore token persistence and atomic rotation.
//! 4. `PipeWire` stream resolution with monotonic serial and node-ID-reuse protection.
//! 5. Coordinate conversions: Compositor space <-> Stream pixels <-> Crop/scale <-> EIS regions.
//! 6. Cursor mode gating: metadata emitted only when explicitly granted.
//! 7. Worker capability isolation invariant: workers receive only `PipeWire` capabilities.
//! 8. X11 host adapter: unconfined security model diagnostics surfaced.
//! 9. Systemd user session environment validation.
//! 10. Compositor qualification matrix evaluation (GNOME, KDE, Hyprland).
//! 11. Fault tolerance: agent retains portal session and cleans up input on worker crash.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use fr_core::{
    input::{DesktopPoint, InputBounds, KeyTransition, PhysicalKey, PointerButton},
    input_submission::{InputSink, Operation, PlatformError, Submission},
};

use frd::linux::{
    CompositorCapability, CompositorFamily, CompositorPoint, CoordinateConversionError,
    CropScaleMapping, CursorManager, CursorMetadata, CursorMetadataRefusal, CursorMode,
    DeviceFlags, EisInputSink, EisRegion, GraphicalSessionEnvironment, PipeWireStreamCapability,
    PipeWireStreamInfo, PlatformSecurityModel, PortalSequenceError, PortalSession, PortalState,
    QUALIFIED_ROWS, RecordingEisPoster, RecordingX11Poster, RestoreOutcome, RestoreTokenManager,
    SessionEnvironmentError, SessionType, StreamPixelPoint, StreamResolution, StreamResolver,
    StreamVerificationError, WorkerIsolationBoundary, X11InputSink, compositor_to_eis,
    compositor_to_stream_pixel, eis_to_compositor, evaluate_compositor, stream_pixel_to_compositor,
};

fn make_test_bounds() -> InputBounds {
    InputBounds::new(DesktopPoint { x: 0, y: 0 }, 1920, 1080).expect("valid bounds")
}

#[test]
fn test_portal_sequence_and_clipboard_before_start() {
    let mut session = PortalSession::new("test-session-1");
    assert_eq!(session.state(), &PortalState::Initial);

    // Step 1: Session created
    session
        .on_session_created("/org/freedesktop/portal/desktop/session/123")
        .expect("session created");

    // Step 2: Select devices (request pointer + keyboard)
    session
        .on_select_devices(DeviceFlags::POINTER.union(DeviceFlags::KEYBOARD))
        .expect("select devices");

    // Step 3: Select sources
    session
        .on_select_sources(CursorMode::Metadata, Some("old-restore-token".into()))
        .expect("select sources");

    // Invariant test: clipboard configured BEFORE start
    session
        .on_configure_clipboard(true)
        .expect("configure clipboard before start");

    // Step 5: Start response
    let resolution = StreamResolution::new(1920, 1080).expect("valid res");
    let stream_info =
        PipeWireStreamInfo::new(42, Some(1001), "token-123", resolution, HashMap::new());

    session
        .on_started(
            DeviceFlags::POINTER.union(DeviceFlags::KEYBOARD),
            CursorMode::Metadata,
            true,
            vec![stream_info],
            Some("replacement-token-456".into()),
        )
        .expect("portal start");

    assert!(session.is_started());
    assert!(session.is_pointer_granted());
    assert!(session.is_keyboard_granted());
    assert!(session.is_clipboard_granted());
    assert_eq!(
        session.replacement_restore_token(),
        Some("replacement-token-456")
    );

    // Invariant: attempting to configure clipboard AFTER start must fail
    let err = session.on_configure_clipboard(false);
    assert_eq!(err, Err(PortalSequenceError::ClipboardConfiguredAfterStart));
}

#[test]
fn test_returned_grants_define_authorization() {
    let mut session = PortalSession::new("test-session-partial-grant");
    session
        .on_session_created("/org/freedesktop/portal/desktop/session/partial")
        .expect("created");

    // Client requests Pointer + Keyboard + Touchscreen
    session
        .on_select_devices(
            DeviceFlags::POINTER
                .union(DeviceFlags::KEYBOARD)
                .union(DeviceFlags::TOUCHSCREEN),
        )
        .expect("devices selected");

    session
        .on_select_sources(CursorMode::Embedded, None)
        .expect("sources selected");

    session
        .on_configure_clipboard(true)
        .expect("clipboard configured");

    // Compositor GRANTS ONLY POINTER, denies keyboard and touchscreen, denies clipboard
    let resolution = StreamResolution::new(1920, 1080).expect("valid res");
    let stream_info = PipeWireStreamInfo::new(10, Some(500), "tok", resolution, HashMap::new());

    session
        .on_started(
            DeviceFlags::POINTER, // Only pointer granted!
            CursorMode::Embedded,
            false, // Clipboard denied!
            vec![stream_info],
            None,
        )
        .expect("started");

    assert!(session.is_pointer_granted());
    assert!(!session.is_keyboard_granted());
    assert!(!session.is_clipboard_granted());

    // Connect EIS input sink with only the granted devices
    let poster = RecordingEisPoster::new();
    let mut eis_sink = EisInputSink::new(
        poster,
        make_test_bounds(),
        DeviceFlags::POINTER, // matching granted devices
        None,
    );

    // Pointer motion succeeds
    let motion_op = Operation::Absolute(DesktopPoint { x: 500, y: 400 });
    assert_eq!(eis_sink.prepare(motion_op), Ok(()));
    assert_eq!(eis_sink.submit(motion_op), Submission::Submitted);

    // Keyboard injection is REFUSED because keyboard was not granted
    let key_op = Operation::Key {
        key: PhysicalKey::new(0x04).expect("valid key"), // 'A'
        transition: KeyTransition::Press,
    };
    assert_eq!(eis_sink.prepare(key_op), Err(PlatformError::Unsupported));
    assert_eq!(
        eis_sink.submit(key_op),
        Submission::NotSubmitted(PlatformError::Unsupported)
    );
}

#[test]
fn test_restore_token_atomic_persistence_and_rotation() {
    let temp_dir = std::env::temp_dir().join(format!("frd_token_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);

    let manager = RestoreTokenManager::new(temp_dir.clone()).expect("manager created");
    let session_id = "desktop-session-alpha";

    // 1. Initial save of a restore token
    manager
        .save_token_atomic(session_id, "initial-token-1111")
        .expect("save token");

    let loaded = manager
        .load_token(session_id)
        .expect("load token")
        .expect("token should exist");
    assert_eq!(loaded.token, "initial-token-1111");

    // 2. Single-use rotation: replacement token provided
    let rotated = manager
        .rotate_token(session_id, Some("replacement-token-2222"))
        .expect("rotate token")
        .expect("new token returned");
    assert_eq!(rotated.token, "replacement-token-2222");

    let loaded2 = manager
        .load_token(session_id)
        .expect("load")
        .expect("exists");
    assert_eq!(loaded2.token, "replacement-token-2222");

    // 3. Single-use rotation: no replacement token provided (token consumed)
    let consumed = manager.rotate_token(session_id, None).expect("rotate none");
    assert!(consumed.is_none());

    let loaded3 = manager.load_token(session_id).expect("load");
    assert!(loaded3.is_none());

    // 4. Compositor rejection handling
    manager
        .save_token_atomic(session_id, "expired-token-3333")
        .expect("save");
    let outcome = manager.handle_rejection(session_id);
    assert_eq!(outcome, RestoreOutcome::RejectedPromptRequired);
    assert!(manager.load_token(session_id).expect("load").is_none());

    let _ = fs::remove_dir_all(temp_dir);
}

#[test]
fn test_pipewire_stream_resolution_and_node_id_reuse() {
    let mut resolver = StreamResolver::new();
    let res = StreamResolution::new(2560, 1440).expect("valid res");

    let original_stream = PipeWireStreamInfo::new(
        64,
        Some(10050),
        "session-a",
        res,
        HashMap::from([("pipewire.serial".into(), "10050".into())]),
    );
    let identifier = original_stream.unique_identifier.clone();
    resolver.register_stream(original_stream);

    // Scenario 1: Exact stream reconnect with same serial and resolution -> OK
    let good_reconnect = PipeWireStreamInfo::new(
        64,
        Some(10050),
        "session-a",
        res,
        HashMap::from([("pipewire.serial".into(), "10050".into())]),
    );
    assert_eq!(resolver.verify_stream(&identifier, &good_reconnect), Ok(()));

    // Scenario 2: Node ID reused by a DIFFERENT stream with different serial -> Refused
    let recycled_node_stream = PipeWireStreamInfo::new(
        64,          // same node ID 64!
        Some(10099), // but different serial 10099!
        "session-b",
        res,
        HashMap::from([("pipewire.serial".into(), "10099".into())]),
    );
    let verify_err = resolver.verify_stream(&identifier, &recycled_node_stream);
    assert_eq!(
        verify_err,
        Err(StreamVerificationError::NodeIdReused {
            node_id: 64,
            expected_serial: Some(10050),
            actual_serial: Some(10099),
        })
    );

    // Scenario 3: Stream resolution unexpectedly changed without reconfiguration -> Refused
    let new_res = StreamResolution::new(1920, 1080).expect("res");
    let changed_res_stream =
        PipeWireStreamInfo::new(64, Some(10050), "session-a", new_res, HashMap::new());
    assert_eq!(
        resolver.verify_stream(&identifier, &changed_res_stream),
        Err(StreamVerificationError::ResolutionChanged {
            expected: res,
            actual: new_res,
        })
    );
}

#[test]
fn test_coordinate_conversions_distinct_systems() {
    let res = StreamResolution::new(1920, 1080).expect("res");
    let mapping = CropScaleMapping::new(0.0, 0.0, 1920.0, 1080.0, 1.0).expect("mapping");

    // 1. Compositor to stream pixel
    let comp_pt = CompositorPoint::new(960.0, 540.0);
    let px = compositor_to_stream_pixel(comp_pt, &mapping, res).expect("pixel");
    assert_eq!(px, StreamPixelPoint { x: 960, y: 540 });

    // Round-trip
    let back_comp = stream_pixel_to_compositor(px, &mapping).expect("compositor");
    assert!((back_comp.x - 960.0).abs() < 1e-6);
    assert!((back_comp.y - 540.0).abs() < 1e-6);

    // Out of bounds check
    let out_of_bounds = CompositorPoint::new(2500.0, 540.0);
    assert!(matches!(
        compositor_to_stream_pixel(out_of_bounds, &mapping, res),
        Err(CoordinateConversionError::OutOfBounds { .. })
    ));

    // Non-finite check
    let nan_pt = CompositorPoint::new(f64::NAN, 540.0);
    assert_eq!(
        compositor_to_stream_pixel(nan_pt, &mapping, res),
        Err(CoordinateConversionError::NonFiniteCoordinate)
    );

    // 2. EIS Region mapping (e.g. scale 1.5, offset (100, 200))
    let region = EisRegion::new(100, 200, 1920, 1080, 1.5).expect("eis region");
    let eis_in = CompositorPoint::new(300.0, 400.0); // rel x = 200, rel y = 200
    let eis_out = compositor_to_eis(eis_in, &region).expect("eis point");
    assert!((eis_out.x - 300.0).abs() < 1e-6); // 200 * 1.5 = 300
    assert!((eis_out.y - 300.0).abs() < 1e-6); // 200 * 1.5 = 300

    let back_from_eis = eis_to_compositor(eis_out, &region).expect("back comp");
    assert!((back_from_eis.x - 300.0).abs() < 1e-6);
    assert!((back_from_eis.y - 400.0).abs() < 1e-6);
}

#[test]
fn test_cursor_mode_gating() {
    // Mode 1: Metadata granted -> queries succeed
    let mut manager = CursorManager::new(CursorMode::Metadata);
    assert!(manager.allows_metadata());

    let meta = CursorMetadata::new(
        10,
        10,
        32,
        32,
        Some(CompositorPoint::new(500.0, 500.0)),
        true,
    );
    manager.update_metadata(meta.clone()).expect("update meta");
    let retrieved = manager.get_metadata().expect("get meta");
    assert_eq!(retrieved.hotspot_x, 10);
    assert_eq!(retrieved.width, 32);

    // Mode 2: Embedded granted -> metadata queries refused to prevent duplicate cursor rendering
    let embedded_manager = CursorManager::new(CursorMode::Embedded);
    assert!(!embedded_manager.allows_metadata());
    assert_eq!(
        embedded_manager.get_metadata(),
        Err(CursorMetadataRefusal::MetadataNotGranted(
            CursorMode::Embedded
        ))
    );

    // Mode 3: Hidden granted -> metadata queries refused
    let hidden_manager = CursorManager::new(CursorMode::Hidden);
    assert!(!hidden_manager.allows_metadata());
    assert_eq!(
        hidden_manager.get_metadata(),
        Err(CursorMetadataRefusal::MetadataNotGranted(
            CursorMode::Hidden
        ))
    );
}

#[test]
fn test_worker_isolation_boundary_invariant() {
    let res = StreamResolution::new(1920, 1080).expect("res");
    let cap = PipeWireStreamCapability {
        node_id: 12,
        serial: Some(999),
        resolution: res,
        pipewire_remote_fd: Some(5),
    };

    // Valid capability contains only stream metadata
    assert!(WorkerIsolationBoundary::verify_worker_capability(&cap).is_ok());

    // From PortalSession: extraction yields only PipeWire capability
    let mut session = PortalSession::new("iso-session");
    session.on_session_created("/dbus/session").expect("c");
    session.on_select_devices(DeviceFlags::POINTER).expect("d");
    session
        .on_select_sources(CursorMode::Embedded, None)
        .expect("s");
    session.on_configure_clipboard(false).expect("cl");
    let stream_info = PipeWireStreamInfo::new(12, Some(999), "tok", res, HashMap::new());
    session
        .on_started(
            DeviceFlags::POINTER,
            CursorMode::Embedded,
            false,
            vec![stream_info],
            None,
        )
        .expect("started");

    let extracted = session
        .extract_worker_capability(0, Some(5))
        .expect("extract");
    assert_eq!(extracted.node_id, 12);
    assert_eq!(extracted.serial, Some(999));
    assert_eq!(extracted.resolution, res);
    assert_eq!(extracted.pipewire_remote_fd, Some(5));

    // Agent retains portal session and remains open
    assert!(session.is_started());
}

#[test]
fn test_x11_host_adapter_security_diagnostics_and_pairs() {
    let poster = RecordingX11Poster::new();
    let mut x11_sink = X11InputSink::new(poster, make_test_bounds());

    // Surfaced security diagnostics
    let diag = x11_sink.security_report();
    assert_eq!(diag.model, PlatformSecurityModel::X11Unconfined);
    assert!(!diag.window_isolation);
    assert!(!diag.consent_prompt_enforced);
    assert!(diag.diagnostic_warning.is_some());

    // Pair requirements for X11
    assert!(x11_sink.repeat_requires_pair());
    assert!(x11_sink.line_scroll_requires_pairs());

    // Pointer injection works
    let pt_op = Operation::Absolute(DesktopPoint { x: 200, y: 300 });
    assert_eq!(x11_sink.prepare(pt_op), Ok(()));
    assert_eq!(x11_sink.submit(pt_op), Submission::Submitted);

    // Button injection works
    let btn_op = Operation::Button {
        button: PointerButton::Primary,
        pressed: true,
    };
    assert_eq!(x11_sink.prepare(btn_op), Ok(()));
    assert_eq!(x11_sink.submit(btn_op), Submission::Submitted);
}

#[test]
fn test_systemd_session_environment_detection() {
    let mut env = GraphicalSessionEnvironment {
        session_type: SessionType::Wayland,
        wayland_display: Some("wayland-0".into()),
        x11_display: None,
        dbus_session_bus_address: Some("unix:path=/run/user/1000/bus".into()),
        current_desktop: Some("GNOME".into()),
        user_runtime_dir: Some(PathBuf::from("/run/user/1000")),
    };

    // Valid Wayland hosting environment
    assert!(env.validate_for_hosting().is_ok());

    // Missing WAYLAND_DISPLAY in Wayland session triggers typed refusal
    env.wayland_display = None;
    assert_eq!(
        env.validate_for_hosting(),
        Err(SessionEnvironmentError::WaylandDisplayMissing)
    );

    // Missing D-Bus session bus triggers typed refusal
    env.wayland_display = Some("wayland-0".into());
    env.dbus_session_bus_address = None;
    env.user_runtime_dir = Some(PathBuf::from("/nonexistent/dir"));
    assert_eq!(
        env.validate_for_hosting(),
        Err(SessionEnvironmentError::DbusSessionBusMissing)
    );
}

#[test]
fn test_no_compositor_is_qualified_without_evidence() {
    // The matrix holds only rows backed by retained evidence; there are none.
    assert_eq!(QUALIFIED_ROWS.len(), 0);
    for family in [
        CompositorFamily::GnomeMutter,
        CompositorFamily::KdeKWin,
        CompositorFamily::HyprlandWlroots,
        CompositorFamily::Other("Enlightenment".into()),
    ] {
        let CompositorCapability::Unsupported { detail } = evaluate_compositor(&family) else {
            panic!("{family:?} must not evaluate as capable without evidence");
        };
        assert!(detail.contains("is not qualified"), "{detail}");
        assert!(detail.contains("not implemented"), "{detail}");
    }
}

#[test]
fn test_agent_owns_portal_session_across_worker_crash() {
    use fr_core::ids::RemoteSessionId;
    use fr_core::time::{HostDuration, HostInstant};
    use frd::session_agent::{
        ApprovalMode, GrantedScope, PeerIdentity, PlatformKind, RequestedScope, SessionAgent,
        SessionRole, SubmissionRefusal,
    };

    let now = HostInstant::from_micros(1_000_000);
    let mut agent = SessionAgent::new(
        ApprovalMode::PromptAlways,
        PlatformKind::LinuxWayland,
        1000,
        make_test_bounds(),
    );

    let session_id = RemoteSessionId::from_raw(101);
    let peer = PeerIdentity {
        node_id: "node-worker-fault".into(),
        node_name: "test-peer".into(),
        user_id: "user-fault".into(),
    };
    let req = RequestedScope {
        role: SessionRole::Controller,
        displays: vec![0],
        audio: frd::session_agent::AudioScope::PlaybackOnly,
        clipboard: true,
        file_transfer: false,
    };

    let _ = agent.request_session(session_id, &peer, &req, now);
    let grant = GrantedScope {
        role: SessionRole::Controller,
        displays: vec![0],
        audio: frd::session_agent::AudioScope::PlaybackOnly,
        clipboard: true,
        file_transfer: false,
        granted_at: now,
        expires_at: now.checked_add(HostDuration::from_millis_checked(5000).expect("dur")),
    };
    agent
        .approve_session(session_id, grant, now)
        .expect("approved");

    // Agent creates and starts PortalSession
    let mut portal = PortalSession::new("fault-session-1");
    portal.on_session_created("/portal/session/1").expect("c");
    portal
        .on_select_devices(DeviceFlags::POINTER.union(DeviceFlags::KEYBOARD))
        .expect("d");
    portal
        .on_select_sources(CursorMode::Embedded, None)
        .expect("s");
    portal.on_configure_clipboard(true).expect("cl");

    let res = StreamResolution::new(1920, 1080).expect("res");
    let stream_info = PipeWireStreamInfo::new(7, Some(101), "tok", res, HashMap::new());
    portal
        .on_started(
            DeviceFlags::POINTER.union(DeviceFlags::KEYBOARD),
            CursorMode::Embedded,
            true,
            vec![stream_info],
            None,
        )
        .expect("started");

    // Agent marks portal permission granted
    agent.permissions_mut().set_permission(
        frd::session_agent::PermissionKind::RemoteDesktopPortal,
        frd::session_agent::PermissionStatus::Granted,
    );

    // Delegate ONLY PipeWire capability to media worker
    let worker_cap = portal
        .extract_worker_capability(0, Some(3))
        .expect("worker cap");
    assert_eq!(worker_cap.node_id, 7);

    // Injection succeeds before worker crash
    let move_op = Operation::Absolute(DesktopPoint { x: 300, y: 300 });
    assert!(
        agent
            .verify_and_track_submission(session_id, &move_op, now)
            .is_ok()
    );

    // WORKER CRASHES: agent handles worker failure
    let crash_time = now
        .checked_add(HostDuration::from_millis_checked(100).expect("dur"))
        .expect("time");
    let crash_report = agent.on_worker_crash(crash_time);
    assert_eq!(crash_report.keys_uncertain, 0);
    assert!(agent.is_revoked());

    // Subsequent injection is refused with Revoked
    assert_eq!(
        agent.verify_and_track_submission(session_id, &move_op, crash_time),
        Err(SubmissionRefusal::Revoked)
    );

    // Agent still retains the PortalSession and can cleanly shut it down
    assert!(portal.is_started());
    portal.close();
    assert_eq!(portal.state(), &PortalState::Closed);
}
