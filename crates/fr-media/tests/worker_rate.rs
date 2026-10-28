use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    worker::{self, Header, Identity, Kind, Record, rate},
};
use std::io::Cursor;

fn request() -> rate::Capture {
    rate::Capture {
        frame: FrameId::from_raw(7),
        observed_micros: 9,
        force_idr: true,
        only_if_changed: true,
        bitrate: 1_000_000,
    }
}
fn unit(kind: FrameKind) -> EncodedAccessUnit {
    EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        FrameId::from_raw(7),
        kind,
        CodecConfigurationGeneration::from_raw(3),
        9,
        vec![1, 2, 3, 4],
    )
    .unwrap()
}
#[test]
fn rate_capture_is_canonical_bounded_and_keeps_original_observation() {
    let expected = [
        0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 9, 3, 0, 15, 66, 64,
    ];
    assert_eq!(request().encode().unwrap(), expected);
    assert_eq!(rate::Capture::decode(&expected), Ok(request()));
    for n in 0..expected.len() {
        assert!(rate::Capture::decode(&expected[..n]).is_err());
    }
    assert!(rate::Capture::decode(&[0; 22]).is_err());
    for flag in 4..=u8::MAX {
        let mut b = expected;
        b[16] = flag;
        assert!(rate::Capture::decode(&b).is_err());
    }
    for bitrate in [0, 9_999, 200_000_001, u32::MAX] {
        assert!(
            rate::Capture {
                bitrate,
                ..request()
            }
            .encode()
            .is_err()
        );
        let mut b = expected;
        b[17..].copy_from_slice(&bitrate.to_be_bytes());
        assert!(rate::Capture::decode(&b).is_err());
    }
    for bitrate in [10_000, 200_000_000] {
        let r = rate::Capture {
            bitrate,
            ..request()
        };
        assert_eq!(rate::Capture::decode(&r.encode().unwrap()), Ok(r));
    }
}
#[test]
fn actual_rate_unit_preserves_all_metadata_and_the_original_allocation_bound() {
    for kind in [
        FrameKind::Idr {
            recovery: RecoveryGeneration::from_raw(5),
        },
        FrameKind::Predicted {
            references: FrameId::from_raw(6),
        },
    ] {
        let unit = unit(kind);
        let body = rate::unit_payload(&unit, 500_000).unwrap();
        assert_eq!(body.len(), worker::UNIT_PREFIX_BYTES + unit.bytes().len());
        assert!(
            worker::parse_unit(body.clone(), &ProtocolLimits::ABSOLUTE).is_err(),
            "legacy kind must not accept actual-rate metadata"
        );
        let allocation = body.capacity();
        let (actual, decoded) = rate::parse_unit(body, &ProtocolLimits::ABSOLUTE).unwrap();
        assert_eq!(actual, 500_000);
        assert_eq!(decoded, unit);
        assert_eq!(decoded.into_bytes().capacity(), allocation);
    }
}
#[test]
fn rate_unit_rejects_padding_invalid_rates_and_impossible_references() {
    let unit = unit(FrameKind::Predicted {
        references: FrameId::from_raw(6),
    });
    let good = rate::unit_payload(&unit, 500_000).unwrap();
    for n in 0..=worker::UNIT_PREFIX_BYTES {
        assert!(rate::parse_unit(good[..n].to_vec(), &ProtocolLimits::ABSOLUTE).is_err());
    }
    for pos in 37..40 {
        let mut b = good.clone();
        b[pos] = 1;
        assert!(rate::parse_unit(b, &ProtocolLimits::ABSOLUTE).is_err());
    }
    for bitrate in [0_u32, 9_999, 200_000_001, u32::MAX] {
        let mut b = good.clone();
        b[33..37].copy_from_slice(&bitrate.to_be_bytes());
        assert!(rate::parse_unit(b, &ProtocolLimits::ABSOLUTE).is_err());
    }
    let mut b = good.clone();
    b[24..32].copy_from_slice(&7_u64.to_be_bytes());
    assert!(rate::parse_unit(b, &ProtocolLimits::ABSOLUTE).is_err());
    assert!(
        rate::parse_unit(
            worker::unit_payload(&unit).unwrap(),
            &ProtocolLimits::ABSOLUTE
        )
        .is_err()
    );
}
#[test]
fn unchanged_reports_actual_rate_without_claiming_the_requested_rate_was_installed() {
    let proof = worker::UnchangedCapture {
        candidate: FrameId::from_raw(7),
        reference: FrameId::from_raw(3),
        observed_micros: 9,
    };
    let b = rate::unchanged_payload(proof, 8_000_000).unwrap();
    assert_eq!(b.len(), rate::UNCHANGED_BYTES);
    assert_eq!(rate::parse_unchanged(&b), Ok((8_000_000, proof)));
    for n in 0..b.len() {
        assert!(rate::parse_unchanged(&b[..n]).is_err());
    }
    let mut trailing = b.clone();
    trailing.push(0);
    assert!(rate::parse_unchanged(&trailing).is_err());
    let mut future = b;
    future[8..16].copy_from_slice(&7_u64.to_be_bytes());
    assert!(rate::parse_unchanged(&future).is_err());
}
#[test]
fn new_kinds_enforce_exact_header_limits_before_body_allocation() {
    let l = ProtocolLimits::ABSOLUTE;
    for (kind, len, code) in [
        (Kind::CaptureAtRate, 21, 20_u16),
        (Kind::RateUnchanged, 28, 279),
    ] {
        let h = Header {
            kind,
            identity: Identity {
                epoch: 1,
                sequence: 2,
            },
            length: len,
        };
        let b = h.encode(&l).unwrap();
        assert_eq!(&b[6..8], &code.to_be_bytes());
        assert_eq!(Header::decode(&b, &l), Ok(h));
        for wrong in [0, len - 1, len + 1, u32::MAX as usize] {
            let mut b = b;
            b[32..36].copy_from_slice(&u32::try_from(wrong).unwrap().to_be_bytes());
            assert_eq!(
                Record::read(&mut Cursor::new(b), &l).unwrap_err(),
                worker::Error::ResourceLimit
            );
        }
    }
    let h = Header {
        kind: Kind::RateUnit,
        identity: Identity {
            epoch: 1,
            sequence: 3,
        },
        length: worker::UNIT_PREFIX_BYTES + l.max_encoded_access_unit_bytes() as usize,
    };
    assert!(h.encode(&l).is_ok());
    assert!(
        Header {
            length: h.length + 1,
            ..h
        }
        .encode(&l)
        .is_err()
    );
    assert!(
        Header {
            length: worker::UNIT_PREFIX_BYTES,
            ..h
        }
        .encode(&l)
        .is_err()
    );
}
