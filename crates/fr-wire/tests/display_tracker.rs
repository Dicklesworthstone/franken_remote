use fr_core::{
    ids::{DisplayGeometryGeneration, OsSessionId},
    input::PointerButton,
    limits::ProtocolLimits,
};
use fr_wire::display::{
    DisplayMappingError,
    tracker::{
        DisplayCatalogTracker, DisplayTargetController, HostMonitorDescriptor,
        MultiDisplayStreamManager,
    },
};

const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;

fn make_os_session() -> OsSessionId {
    OsSessionId::from_raw(0x1234_5678_9ABC_DEF0_1234_5678_9ABC_DEF0)
}

#[test]
fn topology_initialization_and_generation() {
    let mut tracker = DisplayCatalogTracker::new(make_os_session(), LIMITS);
    assert_eq!(
        tracker.geometry_generation(),
        DisplayGeometryGeneration::INITIAL
    );
    assert_eq!(tracker.revision(), 1);
    assert!(tracker.active_catalog().is_empty());

    let m1 = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    let m2 = HostMonitorDescriptor::new("HDMI-1", 1920, 0, 2560, 1440, 2560, 1440, 1, 1, 0);

    let update = tracker.update_topology(&[m1, m2]).unwrap();
    assert!(update.geometry_changed);
    assert_eq!(update.added_count, 2);
    assert_eq!(tracker.geometry_generation().as_raw(), 1);
    assert_eq!(tracker.revision(), 2);
    assert_eq!(tracker.active_catalog().len(), 2);

    let displays = tracker.active_catalog().displays();
    assert_eq!(displays[0].x, 0);
    assert_eq!(displays[0].pixel_width, 1920);
    assert_eq!(displays[1].x, 1920);
    assert_eq!(displays[1].pixel_width, 2560);

    // Each display must have a non-zero, unique 128-bit handle
    assert_ne!(displays[0].handle, 0);
    assert_ne!(displays[1].handle, 0);
    assert_ne!(displays[0].handle, displays[1].handle);

    // Geometry generation matches
    assert_eq!(displays[0].geometry, tracker.geometry_generation());
    assert_eq!(displays[1].geometry, tracker.geometry_generation());

    // Re-applying exact same topology causes no geometry generation change
    let update2 = tracker.update_topology(&[m1, m2]).unwrap();
    assert!(!update2.geometry_changed);
    assert_eq!(tracker.geometry_generation().as_raw(), 1);
    assert_eq!(tracker.revision(), 2);
}

#[test]
fn hotplug_and_connector_id_reuse_assigns_fresh_handles() {
    let mut tracker = DisplayCatalogTracker::new(make_os_session(), LIMITS);
    let m1 = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);

    tracker.update_topology(&[m1]).unwrap();
    let gen1 = tracker.geometry_generation();
    let handle1 = tracker.active_catalog().displays()[0].handle;

    // Unplug DP-1 (empty monitors list)
    let update_unplug = tracker.update_topology(&[]).unwrap();
    assert!(update_unplug.geometry_changed);
    assert_eq!(update_unplug.removed_count, 1);
    let gen2 = tracker.geometry_generation();
    assert!(gen2 > gen1);

    // Old handle is not found in unplugged catalog
    assert!(tracker.active_catalog().find(handle1).is_none());

    // Hotplug new monitor into the SAME connector DP-1 with same resolution
    let m1_new = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    let update_replug = tracker.update_topology(&[m1_new]).unwrap();
    assert!(update_replug.geometry_changed);
    let gen3 = tracker.geometry_generation();
    assert!(gen3 > gen2);

    let handle3 = tracker.active_catalog().displays()[0].handle;

    // Connector/ID reuse invariant: handle3 MUST NOT equal handle1!
    assert_ne!(handle3, handle1);

    // Validating old handle with old geometry generation refuses
    let err_stale = tracker.validate_display_handle(handle1, gen1);
    assert_eq!(err_stale, Err(DisplayMappingError::StaleGeometry));

    // Validating old handle with new geometry generation refuses
    let err_not_found = tracker.validate_display_handle(handle1, gen3);
    assert_eq!(err_not_found, Err(DisplayMappingError::DisplayNotFound));

    // Validating new handle with new geometry succeeds
    let ok = tracker.validate_display_handle(handle3, gen3);
    assert!(ok.is_ok());
    assert_eq!(ok.unwrap().handle, handle3);
}

#[test]
fn multi_display_coordinate_mapping_and_dead_zone_rejection() {
    let mut tracker = DisplayCatalogTracker::new(make_os_session(), LIMITS);
    // Two monitors with an inter-monitor dead zone gap:
    // Monitor 1: (0, 0) to (1920, 1080)
    // Monitor 2: (2500, 0) to (4420, 1080) [Gap from x=1920 to x=2500]
    let m1 = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    let m2 = HostMonitorDescriptor::new("DP-2", 2500, 0, 1920, 1080, 1920, 1080, 1, 1, 0);

    tracker.update_topology(&[m1, m2]).unwrap();
    let geometry_gen = tracker.geometry_generation();

    // Coordinates in Display 1
    let d1 = tracker.map_coordinate(500, 500, geometry_gen).unwrap();
    assert_eq!(d1.x, 0);
    assert!(d1.contains_pixel(500, 500));

    // Coordinates in Display 2
    let d2 = tracker.map_coordinate(2600, 500, geometry_gen).unwrap();
    assert_eq!(d2.x, 2500);
    assert!(d2.contains_pixel(2600, 500));

    // Coordinates in inter-monitor gap (1920 <= x < 2500): MUST REFUSE, never clamp!
    let gap_err = tracker.map_coordinate(2100, 500, geometry_gen);
    assert_eq!(gap_err, Err(DisplayMappingError::UnmappedCoordinate));

    // Coordinates outside to the left (x < 0): MUST REFUSE, never clamp!
    let left_err = tracker.map_coordinate(-10, 500, geometry_gen);
    assert_eq!(left_err, Err(DisplayMappingError::UnmappedCoordinate));

    // Coordinates outside above (y < 0): MUST REFUSE, never clamp!
    let top_err = tracker.map_coordinate(500, -1, geometry_gen);
    assert_eq!(top_err, Err(DisplayMappingError::UnmappedCoordinate));

    // Coordinates outside below (y >= 1080): MUST REFUSE
    let bottom_err = tracker.map_coordinate(500, 1080, geometry_gen);
    assert_eq!(bottom_err, Err(DisplayMappingError::UnmappedCoordinate));
}

#[test]
fn ambiguous_mapping_rejection_on_overlapping_displays() {
    let mut tracker = DisplayCatalogTracker::new(make_os_session(), LIMITS);
    // Overlapping displays: Display 1 from 0 to 1920, Display 2 from 1000 to 2920
    let m1 = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    let m2 = HostMonitorDescriptor::new("DP-2", 1000, 0, 1920, 1080, 1920, 1080, 1, 1, 0);

    tracker.update_topology(&[m1, m2]).unwrap();
    let geometry_gen = tracker.geometry_generation();

    // Coordinates in non-overlapping portion of Display 1 (x < 1000)
    let d1 = tracker.map_coordinate(500, 500, geometry_gen).unwrap();
    assert_eq!(d1.x, 0);

    // Coordinates in non-overlapping portion of Display 2 (x >= 1920)
    let d2 = tracker.map_coordinate(2100, 500, geometry_gen).unwrap();
    assert_eq!(d2.x, 1000);

    // Coordinates in overlapping region (1000 <= x < 1920): MUST REFUSE WITH AMBIGUOUS MAPPING!
    let ambig_err = tracker.map_coordinate(1500, 500, geometry_gen);
    assert_eq!(ambig_err, Err(DisplayMappingError::AmbiguousMapping));
}

#[test]
fn multi_display_stream_manager_suspends_unobserved_displays() {
    let mut tracker = DisplayCatalogTracker::new(make_os_session(), LIMITS);
    let m1 = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    let m2 = HostMonitorDescriptor::new("DP-2", 1920, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    let m3 = HostMonitorDescriptor::new("DP-3", 3840, 0, 1920, 1080, 1920, 1080, 1, 1, 0);

    tracker.update_topology(&[m1, m2, m3]).unwrap();
    let catalog = tracker.active_catalog();
    let h1 = catalog.displays()[0].handle;
    let h2 = catalog.displays()[1].handle;
    let h3 = catalog.displays()[2].handle;

    let mut streams = MultiDisplayStreamManager::new();
    streams.sync_catalog(catalog, 100);

    // All streams initially suspended
    assert!(streams.is_suspended(h1));
    assert!(streams.is_suspended(h2));
    assert!(streams.is_suspended(h3));
    assert_eq!(streams.active_stream_count(), 0);

    // Viewer subscribes to Display 1
    let ch1 = streams.subscribe(h1).unwrap();
    assert_eq!(ch1, 100);
    assert!(streams.is_active(h1));
    assert!(streams.is_suspended(h2));
    assert!(streams.is_suspended(h3));
    assert_eq!(streams.active_stream_count(), 1);

    // Viewer subscribes to Display 2
    let ch2 = streams.subscribe(h2).unwrap();
    assert_eq!(ch2, 101);
    assert!(streams.is_active(h1));
    assert!(streams.is_active(h2));
    assert!(streams.is_suspended(h3));
    assert_eq!(streams.active_stream_count(), 2);

    // Viewer unsubscribes from Display 1 -> Display 1 becomes suspended
    streams.unsubscribe(h1).unwrap();
    assert!(streams.is_suspended(h1));
    assert!(streams.is_active(h2));
    assert_eq!(streams.active_stream_count(), 1);
}

#[test]
fn remove_display_during_drag_refuses_rather_than_misdelivers() {
    let mut tracker = DisplayCatalogTracker::new(make_os_session(), LIMITS);
    let m1 = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    let m2 = HostMonitorDescriptor::new("DP-2", 1920, 0, 1920, 1080, 1920, 1080, 1, 1, 0);

    tracker.update_topology(&[m1, m2]).unwrap();
    let cat1 = *tracker.active_catalog();
    let gen1 = tracker.geometry_generation();
    let h2 = cat1.displays()[1].handle;

    let mut controller = DisplayTargetController::new();

    // Start drag on Display 2 (pointer press at x=2000, y=500)
    let pressed_target = controller
        .on_pointer_press(PointerButton::Primary, 2000, 500, gen1, &cat1)
        .unwrap();
    assert_eq!(pressed_target, h2);
    assert_eq!(controller.explicit_target(), Some(h2));

    // Pointer moves on Display 2 during drag -> succeeds
    let (target, pt) = controller
        .route_pointer_move(2050, 520, gen1, &cat1)
        .unwrap();
    assert_eq!(target, h2);
    assert_eq!(pt.x, 2050);

    // FAULT SIMULATION: Display 2 is unplugged / removed from host!
    tracker.update_topology(&[m1]).unwrap();
    let cat2 = *tracker.active_catalog();
    let gen2 = tracker.geometry_generation();
    assert!(gen2 > gen1);

    // Next drag motion arrives with old geometry G1 -> MUST REFUSE WITH StaleGeometry!
    let move_fault = controller.route_pointer_move(2060, 530, gen1, &cat2);
    assert_eq!(move_fault, Err(DisplayMappingError::StaleGeometry));

    // Button release arrives -> MUST REFUSE rather than misdeliver!
    let release_fault =
        controller.on_pointer_release(PointerButton::Primary, 2060, 530, gen1, &cat2);
    assert_eq!(release_fault, Err(DisplayMappingError::StaleGeometry));

    // And looking up the removed display handle in the new catalog returns DisplayNotFound
    assert_eq!(
        tracker.validate_display_handle(h2, gen2),
        Err(DisplayMappingError::DisplayNotFound)
    );
}

#[test]
fn change_scale_during_click_refuses_stale_geometry() {
    let mut tracker = DisplayCatalogTracker::new(make_os_session(), LIMITS);
    // Display 1 at scale 1.0 (1/1)
    let m1_scale1 = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    tracker.update_topology(&[m1_scale1]).unwrap();
    let cat1 = *tracker.active_catalog();
    let gen1 = tracker.geometry_generation();
    let h1 = cat1.displays()[0].handle;

    let mut controller = DisplayTargetController::new();

    // Mouse button pressed at (500, 500) under scale 1.0 (gen1)
    let target = controller
        .on_pointer_press(PointerButton::Primary, 500, 500, gen1, &cat1)
        .unwrap();
    assert_eq!(target, h1);

    // FAULT SIMULATION: Display 1 scale changes to 1.5 (3/2)
    let m1_scale1_5 = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1280, 720, 3, 2, 0);
    tracker.update_topology(&[m1_scale1_5]).unwrap();
    let cat2 = *tracker.active_catalog();
    let gen2 = tracker.geometry_generation();
    assert!(gen2 > gen1);

    // Button release arrives referencing old geometry G1 -> MUST REFUSE WITH StaleGeometry!
    let release_fault =
        controller.on_pointer_release(PointerButton::Primary, 500, 500, gen1, &cat2);
    assert_eq!(release_fault, Err(DisplayMappingError::StaleGeometry));
}
