use fr_core::{
    ids::{DisplayGeometryGeneration, HostBootId, OsSessionId, RemoteSessionId},
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{
    WireError,
    display::{self, Catalog, Display, Message},
    input::{InputDelivery as T, InputDirection as D},
    negotiation::ControlBinding,
};
const L: ProtocolLimits = ProtocolLimits::ABSOLUTE;
fn parent() -> ControlBinding {
    ControlBinding {
        id: 0x0102_0304,
        host_boot: HostBootId::from_raw(u128::from_be_bytes([0x11; 16])),
        os_session: OsSessionId::from_raw(u128::from_be_bytes([0x22; 16])),
        remote_session: RemoteSessionId::from_raw(u128::from_be_bytes([0x33; 16])),
    }
}
fn screen() -> Display {
    Display {
        handle: u128::from_be_bytes([0x44; 16]),
        geometry: DisplayGeometryGeneration::INITIAL,
        x: -1920,
        y: -20,
        pixel_width: 1920,
        pixel_height: 1080,
        logical_width: 1280,
        logical_height: 720,
        scale_numerator: 3,
        scale_denominator: 2,
        rotation: 1,
    }
}
fn catalog() -> Catalog {
    Catalog::new(7, &[screen()], &L).unwrap()
}
fn fixture(select: bool) -> Vec<u8> {
    let mut b = vec![
        0x46,
        0x52,
        0x44,
        0x30,
        0,
        0,
        0,
        if select { 0x22 } else { 0x20 },
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        if select { 96 } else { 114 },
        1,
        2,
        3,
        4,
        0,
        0,
        0,
        0,
    ];
    for v in [0x11, 0x22, 0x33] {
        b.extend_from_slice(&[v; 16]);
    }
    b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 7]);
    if !select {
        b.push(1);
    }
    b.extend_from_slice(&[0x44; 16]);
    b.extend_from_slice(&[0; 8]);
    if select {
        b.extend_from_slice(&[0; 8]);
    } else {
        b.extend_from_slice(&[0xff, 0xff, 0xf8, 0x80, 0xff, 0xff, 0xff, 0xec]);
    }
    b.extend_from_slice(&[0, 0, 7, 0x80, 0, 0, 4, 0x38]);
    if !select {
        b.extend_from_slice(&[0, 0, 5, 0, 0, 0, 2, 0xd0, 0, 0, 0, 3, 0, 0, 0, 2, 1]);
    }
    b
}
#[test]
fn independent_goldens_and_every_truncation_pin_signed_geometry_and_full_parent() {
    for select in [false, true] {
        let dir = if select {
            D::ViewerToHost
        } else {
            D::HostToViewer
        };
        let m = if select {
            Message::Select(catalog().selection(screen().handle).unwrap())
        } else {
            Message::Catalog(catalog())
        };
        let expected = fixture(select);
        let mut out = [0; display::MAX_CATALOG_BYTES];
        let n = display::encode(&m, parent(), &L, &mut out, dir, T::Reliable).unwrap();
        assert_eq!(&out[..n], expected);
        assert_eq!(
            display::decode(&expected, parent(), &L, dir, T::Reliable),
            Ok(m)
        );
        for n in 0..expected.len() {
            assert!(display::decode(&expected[..n], parent(), &L, dir, T::Reliable).is_err());
        }
        let mut trailing = expected;
        trailing.push(0);
        assert!(display::decode(&trailing, parent(), &L, dir, T::Reliable).is_err());
    }
}
#[test]
fn count_duplicates_empty_catalog_and_output_bounds_are_enforced_before_write() {
    assert!(Catalog::new(1, &[screen(); 9], &L).is_err());
    assert!(Catalog::new(1, &[screen(); 2], &L).is_err());
    assert!(Catalog::new(0, &[], &L).is_err());
    let empty = Catalog::new(1, &[], &L).unwrap();
    assert!(empty.selection(42).is_err());
    let entries: Vec<_> = (1..=8)
        .map(|handle| Display { handle, ..screen() })
        .collect();
    let full = Catalog::new(u64::MAX, &entries, &L).unwrap();
    let mut out = [0xa5; display::MAX_CATALOG_BYTES];
    let len = out.len();
    assert_eq!(
        display::encode(
            &Message::Catalog(full),
            parent(),
            &L,
            &mut out[..len - 1],
            D::HostToViewer,
            T::Reliable
        ),
        Err(WireError::BufferTooSmall)
    );
    assert!(out.iter().all(|b| *b == 0xa5));
    assert_eq!(
        display::encode(
            &Message::Catalog(full),
            parent(),
            &L,
            &mut out,
            D::HostToViewer,
            T::Reliable
        )
        .unwrap(),
        len
    );
    assert_eq!(
        display::decode(&out, parent(), &L, D::HostToViewer, T::Reliable),
        Ok(Message::Catalog(full))
    );
    let l = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(128),
        ..Default::default()
    })
    .unwrap();
    out.fill(0xa5);
    assert!(
        display::encode(
            &Message::Catalog(full),
            parent(),
            &l,
            &mut out,
            D::HostToViewer,
            T::Reliable
        )
        .is_err()
    );
    assert!(out.iter().all(|b| *b == 0xa5));
    for count in [9, 255] {
        let mut malformed = fixture(false);
        malformed[80] = count;
        assert_eq!(
            display::decode(&malformed, parent(), &L, D::HostToViewer, T::Reliable),
            Err(WireError::ResourceLimit)
        );
    }
}
#[test]
fn exact_parent_role_delivery_and_zero_bootstrap_cannot_be_substituted() {
    let p = parent();
    for b in [
        ControlBinding { id: 0, ..p },
        ControlBinding { id: 5, ..p },
        ControlBinding {
            host_boot: HostBootId::from_raw(5),
            ..p
        },
        ControlBinding {
            os_session: OsSessionId::from_raw(5),
            ..p
        },
        ControlBinding {
            remote_session: RemoteSessionId::from_raw(5),
            ..p
        },
    ] {
        assert!(display::decode(&fixture(false), b, &L, D::HostToViewer, T::Reliable).is_err());
    }
    assert_eq!(
        display::decode(&fixture(false), p, &L, D::ViewerToHost, T::Reliable),
        Err(WireError::WrongRole)
    );
    assert_eq!(
        display::decode(&fixture(true), p, &L, D::HostToViewer, T::Reliable),
        Err(WireError::WrongRole)
    );
    assert_eq!(
        display::decode(&fixture(false), p, &L, D::HostToViewer, T::Datagram),
        Err(WireError::WrongChannel)
    );
    let mut b = fixture(false);
    b[16..20].fill(0);
    assert!(
        display::decode(
            &b,
            ControlBinding { id: 0, ..p },
            &L,
            D::HostToViewer,
            T::Reliable
        )
        .is_err()
    );
}
#[test]
fn stale_catalog_and_geometry_or_unapproved_viewport_never_select_a_display() {
    let c = catalog();
    let s = c.selection(screen().handle).unwrap();
    assert_eq!(c.selected(s), Ok(screen()));
    for changed in [
        display::Select { revision: 6, ..s },
        display::Select { handle: 42, ..s },
        display::Select {
            geometry: DisplayGeometryGeneration::from_raw(1),
            ..s
        },
        display::Select { x: 1, ..s },
        display::Select { y: 1, ..s },
        display::Select {
            width: s.width - 1,
            ..s
        },
        display::Select {
            height: s.height + 1,
            ..s
        },
    ] {
        assert!(c.selected(changed).is_err());
    }
}
#[test]
fn dimensions_signed_endpoints_scale_and_rotation_refuse_invalid_geometry() {
    let d = screen();
    for bad in [
        Display { handle: 0, ..d },
        Display {
            pixel_width: 0,
            ..d
        },
        Display {
            logical_width: 0,
            ..d
        },
        Display {
            pixel_height: u32::MAX,
            ..d
        },
        Display { x: i32::MAX, ..d },
        Display { y: i32::MAX, ..d },
        Display {
            scale_numerator: 0,
            ..d
        },
        Display {
            scale_denominator: 0,
            ..d
        },
        Display {
            scale_numerator: 65537,
            ..d
        },
        Display {
            scale_denominator: 65537,
            ..d
        },
        Display { rotation: 4, ..d },
        Display {
            scale_numerator: 33,
            ..d
        },
    ] {
        assert!(Catalog::new(1, &[bad], &L).is_err());
    }
    assert!(
        Display {
            x: i32::MIN,
            y: i32::MIN,
            ..d
        }
        .validate(&L)
        .is_ok()
    );
    assert!(
        Display {
            x: i32::MAX,
            pixel_width: 1,
            ..d
        }
        .validate(&L)
        .is_ok()
    );
    let s = display::Select {
        x: u32::MAX,
        ..catalog().selection(d.handle).unwrap()
    };
    let mut out = [0xa5; display::SELECT_BYTES];
    assert!(
        display::encode(
            &Message::Select(s),
            parent(),
            &L,
            &mut out,
            D::ViewerToHost,
            T::Reliable
        )
        .is_err()
    );
    assert!(out.iter().all(|b| *b == 0xa5));
}
#[test]
fn diagnostics_do_not_contain_display_handles_or_desktop_coordinates() {
    let s = catalog().selection(screen().handle).unwrap();
    assert_eq!(
        format!("{:?}", screen()),
        format!(
            "{:?}",
            Display {
                handle: 99,
                x: 345,
                ..screen()
            }
        )
    );
    assert_eq!(
        format!("{s:?}"),
        format!("{:?}", display::Select { handle: 99, ..s })
    );
    assert!(format!("{:?}", Message::Catalog(catalog())).len() < 80);
}
