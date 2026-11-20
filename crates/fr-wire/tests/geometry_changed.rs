//! Independent protocol fixtures for `0x0021 GeometryChanged` (`PROTOCOL.md` §6).
use fr_core::{
    ids::{
        DisplayGeometryGeneration, HostBootId, OsSessionId, RemoteSessionId,
        ViewportMappingGeneration,
    },
    limits::ProtocolLimits,
};
use fr_wire::{
    WireError,
    display::{Display, DisplayCatalogTracker, HostMonitorDescriptor},
    geometry_changed::{self, GeometryChanged, RECORD_BYTES},
    input::{InputDelivery as Delivery, InputDirection as Direction},
    negotiation::ControlBinding,
};

const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;

fn binding() -> ControlBinding {
    ControlBinding {
        id: 0x0102_0304,
        host_boot: HostBootId::from_raw(u128::from_be_bytes([0x11; 16])),
        os_session: OsSessionId::from_raw(u128::from_be_bytes([0x22; 16])),
        remote_session: RemoteSessionId::from_raw(u128::from_be_bytes([0x33; 16])),
    }
}

fn event() -> GeometryChanged {
    GeometryChanged {
        revision: 2,
        previous_geometry: DisplayGeometryGeneration::from_raw(1),
        display: Display {
            handle: u128::from_be_bytes([0x44; 16]),
            geometry: DisplayGeometryGeneration::from_raw(2),
            x: -1920,
            y: 0,
            pixel_width: 3840,
            pixel_height: 2160,
            logical_width: 1920,
            logical_height: 1080,
            scale_numerator: 2,
            scale_denominator: 1,
            rotation: 0,
        },
        viewport: ViewportMappingGeneration::from_raw(5),
        input_fenced: true,
    }
}

fn fixture() -> Vec<u8> {
    // FRD0, v0, kind 0x0021, flags 0, res 0, payload 130 (0x82), binding 0x01020304, ext 0
    let hex = "
        46 52 44 30 00 00 00 21 00 00 00 00 00 00 00 82 01 02 03 04 00 00 00 00
        11 11 11 11 11 11 11 11 11 11 11 11 11 11 11 11
        22 22 22 22 22 22 22 22 22 22 22 22 22 22 22 22
        33 33 33 33 33 33 33 33 33 33 33 33 33 33 33 33
        00 00 00 00 00 00 00 02
        00 00 00 00 00 00 00 01
        44 44 44 44 44 44 44 44 44 44 44 44 44 44 44 44
        00 00 00 00 00 00 00 02
        ff ff f8 80 00 00 00 00
        00 00 0f 00 00 00 08 70
        00 00 07 80 00 00 04 38
        00 00 00 02 00 00 00 01
        00
        00 00 00 00 00 00 00 05
        01
    ";
    hex.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect()
}

#[test]
fn golden_round_trip_and_truncation_matrix() {
    assert_eq!(RECORD_BYTES, 154);
    let expected = fixture();
    assert_eq!(expected.len(), RECORD_BYTES);

    let mut out = [0_u8; RECORD_BYTES];
    let written = geometry_changed::encode(
        &event(),
        binding(),
        &LIMITS,
        &mut out,
        Direction::HostToViewer,
        Delivery::Reliable,
    )
    .unwrap();
    assert_eq!(written, RECORD_BYTES);
    assert_eq!(out.as_slice(), expected.as_slice());

    let decoded = geometry_changed::decode(
        &expected,
        binding(),
        &LIMITS,
        Direction::HostToViewer,
        Delivery::Reliable,
    )
    .unwrap();
    assert_eq!(decoded, event());

    for cut in 0..expected.len() {
        assert!(
            geometry_changed::decode(
                &expected[..cut],
                binding(),
                &LIMITS,
                Direction::HostToViewer,
                Delivery::Reliable,
            )
            .is_err(),
            "must refuse truncation at {cut}"
        );
    }
}

#[test]
fn enforces_role_channel_input_fence_and_strict_generation_advance() {
    let bytes = fixture();
    // Viewer cannot emit GeometryChanged; datagram channel refused.
    assert_eq!(
        geometry_changed::decode(
            &bytes,
            binding(),
            &LIMITS,
            Direction::ViewerToHost,
            Delivery::Reliable,
        ),
        Err(WireError::WrongRole)
    );
    assert_eq!(
        geometry_changed::decode(
            &bytes,
            binding(),
            &LIMITS,
            Direction::HostToViewer,
            Delivery::Datagram,
        ),
        Err(WireError::WrongChannel)
    );

    // Unfenced coordinate input must be refused on encode and decode.
    let mut unfenced = event();
    unfenced.input_fenced = false;
    let mut out = [0_u8; RECORD_BYTES];
    assert_eq!(
        geometry_changed::encode(
            &unfenced,
            binding(),
            &LIMITS,
            &mut out,
            Direction::HostToViewer,
            Delivery::Reliable,
        ),
        Err(WireError::InvalidValue)
    );
    let mut unfenced_wire = bytes.clone();
    unfenced_wire[RECORD_BYTES - 1] = 0;
    assert_eq!(
        geometry_changed::decode(
            &unfenced_wire,
            binding(),
            &LIMITS,
            Direction::HostToViewer,
            Delivery::Reliable,
        ),
        Err(WireError::InvalidValue)
    );

    // Non-advancing geometry generation (previous >= new) refused.
    let mut stale = event();
    stale.previous_geometry = stale.display.geometry;
    assert_eq!(
        geometry_changed::encode(
            &stale,
            binding(),
            &LIMITS,
            &mut out,
            Direction::HostToViewer,
            Delivery::Reliable,
        ),
        Err(WireError::InvalidValue)
    );
}

#[test]
fn display_catalog_tracker_emits_wire_ready_geometry_changed_on_resize() {
    let mut tracker = DisplayCatalogTracker::new(OsSessionId::from_raw(7), LIMITS);
    let m1 = HostMonitorDescriptor::new("DP-1", 0, 0, 1920, 1080, 1920, 1080, 1, 1, 0);
    let u1 = tracker.update_topology(&[m1]).unwrap();
    let prev_gen = u1.catalog.displays()[0].geometry;

    let m2 = HostMonitorDescriptor::new("DP-1", 0, 0, 2560, 1440, 1280, 720, 2, 1, 0);
    let u2 = tracker.update_topology(&[m2]).unwrap();
    assert!(u2.geometry_changed);
    let handle = u2.catalog.displays()[0].handle;

    let notif = tracker
        .geometry_changed_for(handle, prev_gen, ViewportMappingGeneration::from_raw(3))
        .unwrap();
    assert!(notif.input_fenced);
    assert!(notif.display.geometry.as_raw() > prev_gen.as_raw());

    let mut out = [0_u8; RECORD_BYTES];
    let n = geometry_changed::encode(
        &notif,
        binding(),
        &LIMITS,
        &mut out,
        Direction::HostToViewer,
        Delivery::Reliable,
    )
    .unwrap();
    assert_eq!(
        geometry_changed::decode(
            &out[..n],
            binding(),
            &LIMITS,
            Direction::HostToViewer,
            Delivery::Reliable,
        ),
        Ok(notif)
    );
    // Privacy: Debug output must not expose pixel dimensions or origins.
    let dbg = format!("{notif:?}");
    assert!(!dbg.contains("2560") && !dbg.contains("1440"));
}
