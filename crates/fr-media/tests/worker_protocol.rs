use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    worker::*,
};
use std::io::Cursor;
fn id() -> Identity {
    Identity {
        epoch: 7,
        sequence: 0,
    }
}
fn config() -> Configuration {
    Configuration {
        width: 640,
        height: 360,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 4_000_000,
        max_access_unit_bytes: 1024 * 1024,
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
#[test]
fn independent_header_fixture_and_every_truncation() {
    let h = Header {
        kind: Kind::Capture,
        identity: Identity {
            epoch: 1,
            sequence: 2,
        },
        length: 17,
    };
    let bytes = h.encode(&ProtocolLimits::ABSOLUTE).unwrap();
    assert_eq!(
        bytes,
        [
            70, 82, 87, 48, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0,
            0, 0, 0, 2, 0, 0, 0, 17
        ]
    );
    let record = Record::new(
        Kind::Capture,
        h.identity,
        capture_payload(FrameId::FIRST, 123, true),
        &ProtocolLimits::ABSOLUTE,
    )
    .unwrap();
    let mut encoded = Vec::new();
    record
        .write(&mut encoded, &ProtocolLimits::ABSOLUTE)
        .unwrap();
    for n in 1..encoded.len() {
        assert!(Record::read(&mut Cursor::new(&encoded[..n]), &ProtocolLimits::ABSOLUTE).is_err());
    }
    assert!(
        Record::read(&mut Cursor::new([]), &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .is_none()
    );
}
#[test]
fn oversized_announcements_refuse_before_reading_a_body() {
    let mut h = Header {
        kind: Kind::Present,
        identity: id(),
        length: UNIT_PREFIX_BYTES + 1,
    }
    .encode(&ProtocolLimits::ABSOLUTE)
    .unwrap();
    h[32..].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(
        Record::read(&mut Cursor::new(h), &ProtocolLimits::ABSOLUTE).unwrap_err(),
        Error::ResourceLimit
    );
    h[32..].copy_from_slice(&0_u32.to_be_bytes());
    assert!(Header::decode(&h, &ProtocolLimits::ABSOLUTE).is_err());
}
#[test]
fn role_kinds_and_configurations_are_exact() {
    let c = config();
    assert_eq!(Configuration::decode(&c.encode().unwrap()), Ok(c));
    let geometry = c.codec().unwrap().geometry();
    assert_eq!(
        (geometry.coded_width(), geometry.coded_height()),
        (640, 368)
    );
    assert_eq!((geometry.crop_width(), geometry.crop_height()), (640, 360));
    for c in [
        Configuration { width: 0, ..c },
        Configuration { width: 641, ..c },
        Configuration { fps: 0, ..c },
        Configuration {
            max_access_unit_bytes: u32::MAX,
            ..c
        },
    ] {
        assert!(c.codec().is_err());
    }
    let mut b = config().encode().unwrap();
    b[11] = 1;
    assert_eq!(Configuration::decode(&b), Err(Error::Malformed));
    assert!(Kind::Capture.is_request());
    assert!(!Kind::Presented.is_request());
}
#[test]
fn epoch_sequence_and_exhaustion_never_replay_requests() {
    let mut sequence = Sequence::new(7).unwrap();
    let h = Header {
        kind: Kind::Poll,
        identity: id(),
        length: 0,
    };
    let wrong = Header {
        identity: Identity { epoch: 8, ..id() },
        ..h
    };
    assert_eq!(sequence.accept(wrong), Err(Error::WrongEpoch));
    sequence.accept(h).unwrap();
    assert_eq!(sequence.accept(h), Err(Error::WrongSequence));
    assert_eq!(
        sequence.accept(Header {
            identity: Identity {
                sequence: 2,
                ..id()
            },
            ..h
        }),
        Err(Error::WrongSequence)
    );
    sequence
        .accept(Header {
            identity: Identity {
                sequence: 1,
                ..id()
            },
            ..h
        })
        .unwrap();
}
#[test]
fn encoded_units_keep_all_metadata_without_debugging_content() {
    let l = config().limits().unwrap();
    for kind in [
        FrameKind::Idr {
            recovery: RecoveryGeneration::from_raw(4),
        },
        FrameKind::Predicted {
            references: FrameId::from_raw(1),
        },
    ] {
        let unit = EncodedAccessUnit::new(
            &l,
            FrameId::from_raw(2),
            kind,
            config().generation,
            55,
            vec![10, 20, 30, 40],
        )
        .unwrap();
        let r = Record::new(Kind::Unit, id(), unit_payload(&unit).unwrap(), &l).unwrap();
        assert!(!format!("{r:?}").contains("10, 20"));
        assert_eq!(parse_unit(r.into_body(), &l).unwrap(), unit);
    }
}
#[test]
fn empty_invalid_reference_and_reserved_bits_refuse() {
    let l = config().limits().unwrap();
    assert!(parse_unit(vec![0; UNIT_PREFIX_BYTES], &l).is_err());
    let unit = EncodedAccessUnit::new(
        &l,
        FrameId::from_raw(2),
        FrameKind::Predicted {
            references: FrameId::from_raw(1),
        },
        config().generation,
        55,
        vec![1],
    )
    .unwrap();
    let mut p = unit_payload(&unit).unwrap();
    p[33] = 1;
    assert!(parse_unit(p, &l).is_err());
    let mut p = unit_payload(&unit).unwrap();
    p[24..32].copy_from_slice(&2_u64.to_be_bytes());
    assert!(parse_unit(p, &l).is_err());
}

#[test]
fn unchanged_capture_has_exact_bounded_bytes_and_cannot_reference_the_future() {
    let evidence = UnchangedCapture {
        candidate: FrameId::from_raw(9),
        reference: FrameId::from_raw(2),
        observed_micros: 0x0102_0304_0506_0708,
    };
    let bytes = [
        0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 2, 1, 2, 3, 4, 5, 6, 7, 8,
    ];
    assert_eq!(evidence.encode().unwrap(), bytes);
    assert_eq!(UnchangedCapture::decode(&bytes), Ok(evidence));
    for end in 0..bytes.len() {
        assert_eq!(
            UnchangedCapture::decode(&bytes[..end]),
            Err(Error::Malformed)
        );
    }
    assert!(UnchangedCapture::decode(&[0; 25]).is_err());
    for reference in [9, 10, u64::MAX] {
        assert!(
            UnchangedCapture {
                reference: FrameId::from_raw(reference),
                ..evidence
            }
            .encode()
            .is_err()
        );
        let mut malformed = bytes;
        malformed[8..16].copy_from_slice(&reference.to_be_bytes());
        assert!(UnchangedCapture::decode(&malformed).is_err());
    }
    for (kind, length, code) in [
        (Kind::CaptureIfChanged, 17, 7_u16),
        (Kind::Unchanged, 24, 265),
    ] {
        let h = Header {
            kind,
            identity: id(),
            length,
        };
        let wire = h.encode(&ProtocolLimits::ABSOLUTE).unwrap();
        assert_eq!(&wire[6..8], &code.to_be_bytes());
        assert_eq!(Header::decode(&wire, &ProtocolLimits::ABSOLUTE), Ok(h));
        for length in [0, 1, 16, 18, 23, 25, usize::MAX] {
            assert!(
                Header { length, ..h }
                    .encode(&ProtocolLimits::ABSOLUTE)
                    .is_err()
            );
        }
    }
}
