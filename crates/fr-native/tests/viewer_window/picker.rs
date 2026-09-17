//! Real XCB menu, independent Xlib/XTest events and original TLS/UDP Viewer.
//! `XTest` is instrumentation by the selected local user, not remote approval.
use super::*;
use fr_core::ids::DisplayGeometryGeneration;
use fr_native::display_picker::{DisplayPicker, Error as PickError, Status as PickStatus};
use fr_wire::display::{Catalog, Display, MAX_DISPLAYS};
fn catalog(count: usize) -> Catalog {
    let rows = (0..count)
        .map(|i| Display {
            handle: u128::MAX - u128::try_from(i).unwrap(),
            geometry: DisplayGeometryGeneration::INITIAL,
            x: -320 * i32::try_from(i).unwrap(),
            y: 0,
            pixel_width: 320,
            pixel_height: 240,
            logical_width: 320,
            logical_height: 240,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
        })
        .collect::<Vec<_>>();
    Catalog::new(7, &rows, &ProtocolLimits::ABSOLUTE).unwrap()
}
fn opened(picker: &DisplayPicker) -> u32 {
    let c = picker.control();
    wait(|| c.status() != PickStatus::Opening);
    assert_eq!(c.status(), PickStatus::Mapped);
    c.window().unwrap()
}
fn peer(desktop: &Desktop, id: u32, op: &str) -> String {
    let output = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/viewer_window/picker_peer.py"))
        .args([&desktop.display, &id.to_string(), op])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "picker peer {op}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn selected(picker: &mut DisplayPicker, catalog: &Catalog) -> u128 {
    let mut choice = None;
    wait(|| {
        choice = picker.poll(catalog).unwrap();
        choice.is_some()
    });
    assert!(picker.finish(), "choice must follow native join");
    choice.unwrap()
}
#[test]
fn picker_renders_all_rows_and_selects_the_exact_full_width_alias_after_native_close() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let original = viewer(&runtime);
    let stop = original.control();
    let catalog = catalog(MAX_DISPLAYS);
    let mut picker = DisplayPicker::start(&desktop.display, catalog, stop.clone()).unwrap();
    let old = picker.control();
    let id = opened(&picker);
    assert_eq!(peer(&desktop, id, "pixels"), "drawn");
    assert_eq!(picker.poll(&catalog), Ok(None));
    peer(&desktop, id, "click-7");
    assert_eq!(selected(&mut picker, &catalog), u128::MAX - 7);
    assert_eq!(desktop.peer(id, "exists"), "0");
    assert!(!stop.is_stopped());
    assert_eq!(picker.poll(&catalog), Err(PickError::AlreadyUsed));
    old.cancel();
    drop(picker);
    assert!(
        !stop.is_stopped(),
        "retired picker cannot revoke the viewing session"
    );
    assert_eq!(old.status(), PickStatus::Consumed);
}
#[test]
fn picker_keyboard_needs_a_deliberate_choice_not_a_default_primary_display() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    for gesture in ["second-key", "digit-2"] {
        let original = viewer(&runtime);
        let catalog = catalog(3);
        let mut picker =
            DisplayPicker::start(&desktop.display, catalog, original.control()).unwrap();
        let id = opened(&picker);
        peer(&desktop, id, "enter-only");
        thread::sleep(Duration::from_millis(25));
        assert_eq!(picker.poll(&catalog), Ok(None));
        peer(&desktop, id, gesture);
        assert_eq!(selected(&mut picker, &catalog), u128::MAX - 1);
        assert!(!original.control().is_stopped());
    }
}
#[test]
fn picker_rejects_synthetic_selection_and_cross_row_drag_without_stopping_healthy_session() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let original = viewer(&runtime);
    let catalog = catalog(2);
    let mut picker = DisplayPicker::start(&desktop.display, catalog, original.control()).unwrap();
    let id = opened(&picker);
    for op in ["synthetic", "cross-row", "release-only"] {
        peer(&desktop, id, op);
        thread::sleep(Duration::from_millis(25));
        assert_eq!(picker.poll(&catalog), Ok(None));
        assert!(!original.control().is_stopped());
    }
    picker.control().cancel();
    assert!(original.control().is_stopped());
    wait(|| picker.finish());
}
#[test]
fn picker_lifecycle_changes_fence_only_the_original_attempt_and_retain_cleanup() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    for op in ["close", "unmap", "resize", "destroy", "escape"] {
        let original = viewer(&runtime);
        let foreign = viewer(&runtime);
        let catalog = catalog(2);
        let mut picker =
            DisplayPicker::start(&desktop.display, catalog, original.control()).unwrap();
        let id = opened(&picker);
        if op == "escape" {
            peer(&desktop, id, op);
        } else {
            desktop.peer(id, op);
        }
        wait(|| matches!(picker.control().status(), PickStatus::Stopped(_)));
        assert!(original.control().is_stopped());
        assert!(!foreign.control().is_stopped());
        assert!(picker.poll(&catalog).is_err());
        wait(|| picker.finish());
        assert_eq!(desktop.peer(id, "exists"), "0");
    }
}
#[test]
fn picker_catalog_revision_order_and_geometry_cannot_retarget_a_pending_choice() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    for change in 0..3 {
        let original = viewer(&runtime);
        let catalog = catalog(2);
        let mut picker =
            DisplayPicker::start(&desktop.display, catalog, original.control()).unwrap();
        opened(&picker);
        let mut rows = catalog.displays().to_vec();
        match change {
            0 => {}
            1 => rows.reverse(),
            _ => rows[0].x += 1,
        }
        let changed = Catalog::new(
            if change == 0 { 8 } else { 7 },
            &rows,
            &ProtocolLimits::ABSOLUTE,
        )
        .unwrap();
        assert_eq!(picker.poll(&changed), Err(PickError::CatalogChanged));
        assert!(original.control().is_stopped());
        wait(|| picker.finish());
    }
}
#[test]
fn picker_empty_catalog_failed_open_drop_and_cancelled_original_never_select() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    for (display, catalog, error) in [
        (
            desktop.display.as_str(),
            catalog(0),
            PickError::EmptyCatalog,
        ),
        ("remote:0", catalog(1), PickError::InvalidDisplay),
    ] {
        let original = viewer(&runtime);
        assert!(
            matches!(DisplayPicker::start(display, catalog, original.control()), Err(e) if e == error)
        );
        assert!(original.control().is_stopped());
    }
    let original = viewer(&runtime);
    let catalog = catalog(1);
    let mut absent = DisplayPicker::start(":65534", catalog, original.control()).unwrap();
    wait(|| absent.finish());
    assert_eq!(absent.poll(&catalog), Err(PickError::NativeFailure));
    assert!(original.control().is_stopped());
    let original = viewer(&runtime);
    let picker = DisplayPicker::start(&desktop.display, catalog, original.control()).unwrap();
    let id = opened(&picker);
    drop(picker);
    assert!(original.control().is_stopped());
    wait(|| desktop.peer(id, "exists") == "0");
    assert!(matches!(
        DisplayPicker::start(&desktop.display, catalog, original.control()),
        Err(PickError::SessionEnded)
    ));
}
#[test]
fn picker_session_cancellation_wins_even_after_a_native_decision_before_consumption() {
    let desktop = Desktop::start();
    let runtime = support::runtime();
    let original = viewer(&runtime);
    let catalog = catalog(1);
    let mut picker = DisplayPicker::start(&desktop.display, catalog, original.control()).unwrap();
    let id = opened(&picker);
    peer(&desktop, id, "click-0");
    wait(|| picker.control().status() == PickStatus::Decided);
    original.control().stop();
    assert_eq!(picker.poll(&catalog), Err(PickError::SessionEnded));
    wait(|| picker.finish());
}
