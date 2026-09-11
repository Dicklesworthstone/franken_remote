use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::worker::{self, capture::*, *};
use std::io::Cursor;
fn configuration() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: 1_048_576,
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
fn screen(index: u32) -> Screen {
    Screen {
        index,
        root: 100 + u64::from(index),
        width: 320,
        height: 240,
    }
}
#[test]
fn catalog_and_selection_have_exact_independent_bytes() {
    let screens = Screens::new(&[screen(0), screen(1)]).unwrap();
    let expected = vec![
        2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 100, 0, 0, 1, 64, 0, 0, 0, 240, 0, 0, 0, 1, 0, 0, 0, 0,
        0, 0, 0, 101, 0, 0, 1, 64, 0, 0, 0, 240,
    ];
    assert_eq!(screens.encode(), expected);
    assert_eq!(Screens::decode(&expected).unwrap(), screens);
    let body = configure(configuration(), screen(1)).unwrap();
    assert_eq!(body.len(), CONFIGURE_BYTES);
    assert_eq!(&body[28..], &expected[21..]);
    assert_eq!(parse_configuration(&body), Ok((configuration(), screen(1))));
    for n in 0..body.len() {
        assert!(parse_configuration(&body[..n]).is_err());
    }
    let mut trailing = body;
    trailing.push(0);
    assert!(parse_configuration(&trailing).is_err());
}
#[test]
fn malformed_native_identity_count_and_geometry_never_form_a_catalog() {
    let valid = Screens::new(&[screen(0), screen(1)]).unwrap().encode();
    for n in 0..valid.len() {
        assert!(Screens::decode(&valid[..n]).is_err());
    }
    let mut trailing = valid.clone();
    trailing.push(0);
    assert!(Screens::decode(&trailing).is_err());
    for count in [0, 9, 255] {
        let mut b = valid.clone();
        b[0] = count;
        assert!(Screens::decode(&b).is_err());
    }
    for bad in [
        Screen {
            index: 8,
            ..screen(0)
        },
        Screen {
            root: 0,
            ..screen(0)
        },
        Screen {
            root: u64::MAX,
            ..screen(0)
        },
        Screen {
            width: 0,
            ..screen(0)
        },
        Screen {
            width: 321,
            ..screen(0)
        },
        Screen {
            height: 8194,
            ..screen(0)
        },
    ] {
        assert!(Screens::new(&[bad]).is_err());
    }
    assert!(Screens::new(&[screen(0), screen(0)]).is_err());
    assert!(
        Screens::new(&[
            screen(0),
            Screen {
                root: 100,
                ..screen(1)
            }
        ])
        .is_err()
    );
    assert!(
        Screens::new(&[
            screen(0),
            Screen {
                index: 0,
                ..screen(1)
            }
        ])
        .is_err()
    );
    assert_eq!(
        configure(
            configuration(),
            Screen {
                width: 322,
                ..screen(0)
            }
        ),
        Err(Error::GeometryChanged)
    );
    let mut body = configure(configuration(), screen(0)).unwrap();
    body[40..44].copy_from_slice(&322u32.to_be_bytes());
    assert_eq!(parse_configuration(&body), Err(Error::GeometryChanged));
}
#[test]
fn discovery_headers_bound_native_metadata_before_body_allocation() {
    let limits = ProtocolLimits::ABSOLUTE;
    for (kind, length) in [
        (Kind::DiscoverCapture, 0),
        (Kind::CaptureScreens, 21),
        (Kind::ConfigureCapture, 48),
        (Kind::CaptureReady, 48),
    ] {
        let h = Header {
            kind,
            identity: Identity {
                epoch: 7,
                sequence: 0,
            },
            length,
        };
        let b = h.encode(&limits).unwrap();
        assert_eq!(&b[6..8], &(kind as u16).to_be_bytes());
        assert_eq!(Header::decode(&b, &limits), Ok(h));
        for invalid in [1, 20, 22, 47, 49, MAX_CATALOG_BYTES + 1, usize::MAX] {
            if invalid == length {
                continue;
            }
            assert!(
                Header {
                    length: invalid,
                    ..h
                }
                .encode(&limits)
                .is_err()
            );
        }
        let mut large = b;
        large[32..].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(
            Record::read(&mut Cursor::new(large), &limits).unwrap_err(),
            Error::ResourceLimit
        );
    }
    assert_eq!(
        Screens::new(&(0..8).map(screen).collect::<Vec<_>>())
            .unwrap()
            .encode()
            .len(),
        MAX_CATALOG_BYTES
    );
}
#[test]
fn discovery_sequences_never_reset_and_debug_excludes_native_roots() {
    let mut sequence = Sequence::new(7).unwrap();
    let header = Header {
        kind: Kind::DiscoverCapture,
        identity: Identity {
            epoch: 7,
            sequence: 0,
        },
        length: 0,
    };
    sequence.accept(header).unwrap();
    assert!(sequence.accept(header).is_err());
    let a = Screens::new(&[screen(0)]).unwrap();
    let b = Screens::new(&[Screen {
        root: 999_999,
        ..screen(0)
    }])
    .unwrap();
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    assert_eq!(format!("{:?}", screen(0)), format!("{:?}", screen(1)));
    assert!(worker::capture::Screens::new(&[]).is_err());
}
