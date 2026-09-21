#![forbid(unsafe_code)]
//! Deterministic end-to-end lab qualification tests for display selection,
//! multi-display coordinate mapping, geometry generations, and hotplug fault recovery.
//!
//! Conforms to plan sections 15.5, 24.2 and bead `fr-p2-display-selection-mo5`:
//! 1. Lab tests for stale-geometry input rejection (`Refusal::StaleView` / `StaleMapping`).
//! 2. Hotplug fault test: Remove display during a drag (refusal rather than misdelivery).
//! 3. Hotplug fault test: Change scale during a click (refusal of stale geometry).
//! 4. Connector / numeric ID reuse neutralization via unique 128-bit handles.
//! 5. Multi-display coordinate routing: inter-monitor dead zones and overlapping displays refuse.
//! 6. Shared session budget: only selected displays transmitted, unobserved displays suspended.
//! 7. Multi-display selection round-trip across client shell toolbar and wire protocol.

use fr_client::{session::ClientSession, session::ReconnectPolicy, toolbar::ToolbarModel};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, InputLeaseId,
        InputTicketId, OsSessionId, RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{
        DesktopPoint, InputBounds, InputCredentials, InputEvent, InputRequest, InputView,
        PointerButton,
    },
    input_submission::{
        Capabilities, Capability, InputSession, InputSink, Operation, PlatformError, Refusal,
        Submission,
    },
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_wire::{
    display::{
        self, DisplayMappingError, Message,
        tracker::{
            DisplayCatalogTracker, DisplayTargetController, HostMonitorDescriptor,
            MultiDisplayStreamManager,
        },
    },
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
};

const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;

struct TestSink {
    submitted: Vec<Operation>,
}

impl InputSink for TestSink {
    fn prepare(&mut self, _operation: Operation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, operation: Operation) -> Submission {
        self.submitted.push(operation);
        Submission::Submitted
    }
}

fn setup_input_session(
    session_id: RemoteSessionId,
    lease_id: InputLeaseId,
    ticket_id: InputTicketId,
    geometry: DisplayGeometryGeneration,
    bounds: InputBounds,
    now: HostInstant,
) -> InputSession {
    let mut authority = SessionAuthority::new(session_id, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    authority.mark_view_ready(now).unwrap();
    authority.grant_lease(lease_id, now).unwrap();
    authority
        .issue_input_ticket(lease_id, ticket_id, now)
        .unwrap();

    let credentials = InputCredentials {
        session: session_id,
        lease: lease_id,
        ticket: ticket_id,
        view: InputView {
            geometry,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };

    let capabilities = Capabilities::default()
        .with(Capability::Absolute)
        .with(Capability::Buttons);

    InputSession::new(authority, credentials, bounds, capabilities, now).unwrap()
}

#[test]
fn stale_geometry_input_rejection_at_host_boundary() {
    let session_id = RemoteSessionId::from_raw(100);
    let lease_id = InputLeaseId::from_raw(200);
    let ticket_id = InputTicketId::from_raw(300);
    let initial_gen = DisplayGeometryGeneration::from_raw(1);
    let bounds = InputBounds::new(DesktopPoint { x: 0, y: 0 }, 1920, 1080).unwrap();
    let now = HostInstant::from_micros(1_000);

    let mut session =
        setup_input_session(session_id, lease_id, ticket_id, initial_gen, bounds, now);
    let mut sink = TestSink {
        submitted: Vec::new(),
    };

    // 1. Submit pointer input with valid initial geometry generation (G1)
    let event = InputEvent::Pointer {
        position: DesktopPoint { x: 500, y: 500 },
    };
    let credentials_g1 = session.ticket_credentials(ticket_id);
    let request_g1 = InputRequest {
        credentials: credentials_g1,
        sequence: 0,
        event,
    };

    let dispatch_res = session.dispatch(request_g1, &mut sink, || now);
    assert!(dispatch_res.is_ok());
    assert_eq!(sink.submitted.len(), 1); // Absolute position

    // 2. Topology changes on host (e.g. display resolution change or hotplug)
    // Host advances DisplayGeometryGeneration from G1 to G2.
    let stale_gen = initial_gen;
    let fresh_gen = initial_gen.next().unwrap();
    assert!(fresh_gen.supersedes(stale_gen));

    // 3. Client attempts to submit input referencing the STALE geometry generation (G1)
    let stale_credentials = InputCredentials {
        session: session_id,
        lease: lease_id,
        ticket: ticket_id,
        view: InputView {
            geometry: stale_gen,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };

    // If host session has been updated to expect fresh_gen:
    // Create an updated session expecting fresh_gen
    let mut updated_session =
        setup_input_session(session_id, lease_id, ticket_id, fresh_gen, bounds, now);

    let stale_request = InputRequest {
        credentials: stale_credentials,
        sequence: 1,
        event,
    };

    // Invariant: Stale geometry input MUST be refused with Refusal::StaleView!
    let stale_res = updated_session.dispatch(stale_request, &mut sink, || now);
    match stale_res {
        Ok(fr_core::input_submission::Dispatch::Completed(receipt)) => {
            assert_eq!(receipt.refusal, Some(Refusal::StaleView));
            assert_eq!(receipt.submitted_operations, 0);
            assert_eq!(
                receipt.outcome,
                fr_core::input_sequence::InputOutcome::RejectedBeforeSubmission
            );
        }
        other => panic!("expected Completed receipt with StaleView refusal, got {other:?}"),
    }
    assert_eq!(sink.submitted.len(), 1); // No new operation submitted
    eprintln!("[StateTrace: Stale geometry G1 rejected on host expecting G2: Refusal::StaleView]");

    // 4. Client updates to fresh geometry generation (G2):
    // Per Plan §7.2, stale view refusal is terminal on the previous owner.
    // A fresh input session bound to G2 is established, and input with G2 succeeds!
    let fresh_ticket = InputTicketId::from_raw(400);
    let mut fresh_session =
        setup_input_session(session_id, lease_id, fresh_ticket, fresh_gen, bounds, now);

    let fresh_credentials = fresh_session.ticket_credentials(fresh_ticket);
    let fresh_request = InputRequest {
        credentials: fresh_credentials,
        sequence: 0,
        event,
    };

    let fresh_res = fresh_session.dispatch(fresh_request, &mut sink, || now);
    match fresh_res {
        Ok(fr_core::input_submission::Dispatch::Completed(receipt)) => {
            assert_eq!(receipt.refusal, None);
            assert_eq!(receipt.submitted_operations, 1);
        }
        other => panic!("expected Completed receipt, got {other:?}"),
    }
    assert_eq!(sink.submitted.len(), 2);
}

#[test]
fn remove_display_during_drag_refuses_rather_than_misdelivering() {
    let os_session = OsSessionId::from_raw(0x42);
    let mut tracker = DisplayCatalogTracker::new(os_session, LIMITS);

    // Host has two monitors:
    // Display 1: DP-1 at (0, 0, 1920, 1080)
    // Display 2: DP-2 at (1920, 0, 1920, 1080)
    let m1 = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    let m2 = HostMonitorDescriptor::new("DP-2", 1920, 0, 1920, 1080, 1920, 1080, 1, 1, 0);

    tracker.update_topology(&[m1, m2]).unwrap();
    let cat1 = *tracker.active_catalog();
    let gen1 = tracker.geometry_generation();
    let h2 = cat1.displays()[1].handle;

    let mut controller = DisplayTargetController::new();

    // Controller starts drag on Display 2 (button press at x=2500, y=400)
    let target = controller
        .on_pointer_press(PointerButton::Primary, 2500, 400, gen1, &cat1)
        .unwrap();
    assert_eq!(target, h2);
    assert_eq!(controller.explicit_target(), Some(h2));

    // Drag move on Display 2 succeeds while display is present
    let (tgt, pt) = controller
        .route_pointer_move(2550, 420, gen1, &cat1)
        .unwrap();
    assert_eq!(tgt, h2);
    assert_eq!(pt.x, 2550);

    // FAULT TEST: Display 2 is unplugged during the drag!
    tracker.update_topology(&[m1]).unwrap();
    let cat2 = *tracker.active_catalog();
    let gen2 = tracker.geometry_generation();
    assert!(gen2 > gen1);

    // Drag motion arrives referencing old geometry G1 -> refused with StaleGeometry!
    let move_fault = controller.route_pointer_move(2600, 440, gen1, &cat2);
    assert_eq!(move_fault, Err(DisplayMappingError::StaleGeometry));
    eprintln!("[StateTrace: Drag movement after display unplug refused: StaleGeometry]");

    // Button release arrives referencing old geometry G1 -> refused with StaleGeometry!
    let release_fault =
        controller.on_pointer_release(PointerButton::Primary, 2600, 440, gen1, &cat2);
    assert_eq!(release_fault, Err(DisplayMappingError::StaleGeometry));
    eprintln!("[StateTrace: Drag release after display unplug refused: StaleGeometry]");

    // Critical Invariant: The drag action was NEVER misdelivered to Display 1!
    // And looking up the unplugged display handle in the active catalog fails
    assert_eq!(
        tracker.validate_display_handle(h2, gen2),
        Err(DisplayMappingError::DisplayNotFound)
    );
}

#[test]
fn change_scale_during_click_refuses_stale_geometry() {
    let os_session = OsSessionId::from_raw(0x43);
    let mut tracker = DisplayCatalogTracker::new(os_session, LIMITS);

    // Display 1 at scale 1.0 (1/1)
    let m1_1x = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    tracker.update_topology(&[m1_1x]).unwrap();
    let cat1 = *tracker.active_catalog();
    let gen1 = tracker.geometry_generation();
    let h1 = cat1.displays()[0].handle;

    let mut controller = DisplayTargetController::new();

    // Mouse button pressed on Display 1 (press at x=500, y=500) under scale 1.0
    let target = controller
        .on_pointer_press(PointerButton::Primary, 500, 500, gen1, &cat1)
        .unwrap();
    assert_eq!(target, h1);

    // FAULT TEST: Display 1 scale changes to 1.5 (3/2) during the click!
    let m1_1_5x = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1280, 720, 3, 2, 0);
    tracker.update_topology(&[m1_1_5x]).unwrap();
    let cat2 = *tracker.active_catalog();
    let gen2 = tracker.geometry_generation();
    assert!(gen2 > gen1);

    // Button release arrives referencing old geometry G1 -> MUST REFUSE WITH StaleGeometry!
    let release_fault =
        controller.on_pointer_release(PointerButton::Primary, 500, 500, gen1, &cat2);
    assert_eq!(release_fault, Err(DisplayMappingError::StaleGeometry));
    eprintln!("[StateTrace: Click release after scale change refused: StaleGeometry]");
}

#[test]
fn connector_id_reuse_after_hotplug_is_neutralized() {
    let os_session = OsSessionId::from_raw(0x44);
    let mut tracker = DisplayCatalogTracker::new(os_session, LIMITS);

    // Monitor 1 on connector "DP-1"
    let m1 = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    tracker.update_topology(&[m1]).unwrap();
    let gen1 = tracker.geometry_generation();
    let h1 = tracker.active_catalog().displays()[0].handle;

    // Unplug "DP-1"
    tracker.update_topology(&[]).unwrap();
    let gen2 = tracker.geometry_generation();
    assert!(gen2 > gen1);

    // Re-plug new monitor on SAME connector "DP-1" with identical dimensions
    let m1_replug = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    tracker.update_topology(&[m1_replug]).unwrap();
    let gen3 = tracker.geometry_generation();
    assert!(gen3 > gen2);

    let h3 = tracker.active_catalog().displays()[0].handle;

    // Invariant: Connector reuse produces a fresh 128-bit handle.
    // An old handle cannot collide or address the new monitor!
    assert_ne!(h1, h3);
    assert_eq!(
        tracker.validate_display_handle(h1, gen3),
        Err(DisplayMappingError::DisplayNotFound)
    );
    assert!(tracker.validate_display_handle(h3, gen3).is_ok());
}

#[test]
fn multi_display_coordinate_routing_and_dead_zone_rejection() {
    let os_session = OsSessionId::from_raw(0x45);
    let mut tracker = DisplayCatalogTracker::new(os_session, LIMITS);

    // Two displays with an inter-monitor dead zone gap and negative coordinates:
    // Display 1: (-1920, 0) to (0, 1080)
    // Display 2: (500, 0) to (2420, 1080)
    // Dead zone: from x=0 to x=500
    let m1 = HostMonitorDescriptor::new("DP-1", -1920, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    let m2 = HostMonitorDescriptor::new("DP-2", 500, 0, 1920, 1080, 1920, 1080, 1, 1, 0);

    tracker.update_topology(&[m1, m2]).unwrap();
    let geometry_gen = tracker.geometry_generation();

    // 1. Coordinates inside Display 1 map unambiguously
    let d1 = tracker.map_coordinate(-1000, 500, geometry_gen).unwrap();
    assert_eq!(d1.x, -1920);

    // 2. Coordinates inside Display 2 map unambiguously
    let d2 = tracker.map_coordinate(1000, 500, geometry_gen).unwrap();
    assert_eq!(d2.x, 500);

    // 3. Coordinates in dead zone gap (x=250) MUST REFUSE with UnmappedCoordinate, never clamp!
    let gap_err = tracker.map_coordinate(250, 500, geometry_gen);
    assert_eq!(gap_err, Err(DisplayMappingError::UnmappedCoordinate));

    // 4. Coordinates outside left (x = -2000) MUST REFUSE
    let left_err = tracker.map_coordinate(-2000, 500, geometry_gen);
    assert_eq!(left_err, Err(DisplayMappingError::UnmappedCoordinate));

    // 5. Overlapping displays test
    let m1_ov = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    let m2_ov = HostMonitorDescriptor::new("DP-2", 1000, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    tracker.update_topology(&[m1_ov, m2_ov]).unwrap();
    let gen_ov = tracker.geometry_generation();

    // Coordinates in overlap region (x=1500) MUST REFUSE with AmbiguousMapping
    let ambig_err = tracker.map_coordinate(1500, 500, gen_ov);
    assert_eq!(ambig_err, Err(DisplayMappingError::AmbiguousMapping));
}

#[test]
fn shared_session_budget_unobserved_displays_suspended() {
    let os_session = OsSessionId::from_raw(0x46);
    let mut tracker = DisplayCatalogTracker::new(os_session, LIMITS);

    let m1 = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    let m2 = HostMonitorDescriptor::new("DP-2", 1920, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    let m3 = HostMonitorDescriptor::new("DP-3", 3840, 0, 1920, 1080, 1920, 1080, 1, 1, 0);

    tracker.update_topology(&[m1, m2, m3]).unwrap();
    let catalog = tracker.active_catalog();
    let h1 = catalog.displays()[0].handle;
    let h2 = catalog.displays()[1].handle;
    let h3 = catalog.displays()[2].handle;

    let mut stream_mgr = MultiDisplayStreamManager::new();
    stream_mgr.sync_catalog(catalog, 100);

    // Invariant: Unobserved displays are suspended
    assert!(stream_mgr.is_suspended(h1));
    assert!(stream_mgr.is_suspended(h2));
    assert!(stream_mgr.is_suspended(h3));
    assert_eq!(stream_mgr.active_stream_count(), 0);

    // Client selects Display 1: stream 1 becomes active
    let ch1 = stream_mgr.subscribe(h1).unwrap();
    assert_eq!(ch1, 100);
    assert!(stream_mgr.is_active(h1));
    assert!(stream_mgr.is_suspended(h2));
    assert!(stream_mgr.is_suspended(h3));
    assert_eq!(stream_mgr.active_stream_count(), 1);

    // Client switches to Display 2: Display 2 activated, Display 1 unsubscribed
    stream_mgr.subscribe(h2).unwrap();
    stream_mgr.unsubscribe(h1).unwrap();
    assert!(stream_mgr.is_suspended(h1));
    assert!(stream_mgr.is_active(h2));
    assert!(stream_mgr.is_suspended(h3));
    assert_eq!(stream_mgr.active_stream_count(), 1);
}

#[test]
fn multi_display_selection_roundtrip_shell_and_wire() {
    let os_session = OsSessionId::from_raw(0x47);
    let mut tracker = DisplayCatalogTracker::new(os_session, LIMITS);

    let m1 = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    let m2 = HostMonitorDescriptor::new("HDMI-1", 1920, 0, 2560, 1440, 2560, 1440, 1, 1, 0);

    tracker.update_topology(&[m1, m2]).unwrap();
    let host_catalog = *tracker.active_catalog();
    let h2 = host_catalog.displays()[1].handle;

    // 1. Host encodes DisplayCatalog wire message
    let binding = ControlBinding {
        id: 10,
        host_boot: HostBootId::from_raw(1),
        os_session,
        remote_session: RemoteSessionId::from_raw(99),
    };
    let mut wire_buffer = [0u8; display::MAX_CATALOG_BYTES];
    let catalog_msg = Message::Catalog(host_catalog);
    let encoded_len = display::encode(
        &catalog_msg,
        binding,
        &LIMITS,
        &mut wire_buffer,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert!(encoded_len > 0);

    // 2. Client decodes DisplayCatalog wire message
    let decoded_msg = display::decode(
        &wire_buffer[..encoded_len],
        binding,
        &LIMITS,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();

    let Message::Catalog(client_catalog) = decoded_msg else {
        panic!("expected Catalog message");
    };
    assert_eq!(client_catalog.len(), 2);
    assert_eq!(client_catalog.find(h2).unwrap().pixel_width, 2560);

    // 3. ClientSession updates catalog and selects Display 2
    let mut client_session = ClientSession::new(ReconnectPolicy::default());
    client_session.on_catalog_received(client_catalog).unwrap();
    client_session.select_display(h2).unwrap();
    client_session.set_target_display(h2).unwrap();

    assert_eq!(client_session.target_display(), Some(h2));
    assert_eq!(client_session.selected_displays(), &[h2]);

    // 4. Client ToolbarModel reflects Display 2 as explicit target
    let mut toolbar = ToolbarModel::new();
    toolbar.update_from_session(&client_session);
    assert_eq!(toolbar.selected_display, Some(h2));
    assert_eq!(toolbar.target_display, Some(h2));
    assert_eq!(toolbar.display_count, 2);

    let status = toolbar.status_line();
    assert!(status.contains(&format!("{h2} (Target, 1/2)")));

    // 5. Client encodes SelectDisplay wire message for Display 2
    let select_req = client_catalog.selection(h2).unwrap();
    let select_msg = Message::Select(select_req);
    let mut select_buffer = [0u8; display::MAX_CATALOG_BYTES];
    let select_len = display::encode(
        &select_msg,
        binding,
        &LIMITS,
        &mut select_buffer,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert!(select_len > 0);

    // 6. Host receives and decodes SelectDisplay wire message
    let host_decoded = display::decode(
        &select_buffer[..select_len],
        binding,
        &LIMITS,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();

    let Message::Select(host_select) = host_decoded else {
        panic!("expected Select message");
    };
    let confirmed_display = host_catalog.selected(host_select).unwrap();
    assert_eq!(confirmed_display.handle, h2);
    assert_eq!(confirmed_display.pixel_width, 2560);
    assert_eq!(confirmed_display.pixel_height, 1440);
}
