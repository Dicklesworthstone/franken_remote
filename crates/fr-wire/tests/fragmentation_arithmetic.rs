//! Fragmentation arithmetic, range calculations, overflow checks, and boundary tests.
use fr_core::limits::ProtocolLimits;
use fr_wire::*;

fn limits() -> MediaLimits {
    MediaLimits::new(ProtocolLimits::ABSOLUTE, 1_150, 16_384, 64).unwrap()
}

#[test]
fn fragment_count_exact_inexact_and_single_byte() {
    // 1. Single byte
    let d1 = FrameDescriptor {
        frame: 0,
        total_bytes: 1,
        stride: 100,
        capture_micros: 0,
        reference: None,
    };
    assert_eq!(d1.fragment_count(), Ok(1));

    // 2. Exact multiple
    let d_exact = FrameDescriptor {
        frame: 1,
        total_bytes: 1_000,
        stride: 100,
        capture_micros: 0,
        reference: Some(0),
    };
    assert_eq!(d_exact.fragment_count(), Ok(10));

    // 3. Inexact multiple (1 byte over)
    let d_plus_one = FrameDescriptor {
        frame: 2,
        total_bytes: 1_001,
        stride: 100,
        capture_micros: 0,
        reference: Some(1),
    };
    assert_eq!(d_plus_one.fragment_count(), Ok(11));

    // 4. Stride equals total bytes
    let d_same = FrameDescriptor {
        frame: 3,
        total_bytes: 500,
        stride: 500,
        capture_micros: 0,
        reference: Some(2),
    };
    assert_eq!(d_same.fragment_count(), Ok(1));

    // 5. Stride of 1 byte
    let d_stride_one = FrameDescriptor {
        frame: 4,
        total_bytes: 10,
        stride: 1,
        capture_micros: 0,
        reference: Some(3),
    };
    assert_eq!(d_stride_one.fragment_count(), Ok(10));
}

#[test]
fn fragment_count_zero_bytes_or_zero_stride_fails() {
    let d_zero_bytes = FrameDescriptor {
        frame: 0,
        total_bytes: 0,
        stride: 100,
        capture_micros: 0,
        reference: None,
    };
    assert_eq!(
        d_zero_bytes.fragment_count(),
        Err(WireError::InvalidFragment)
    );

    let d_zero_stride = FrameDescriptor {
        frame: 0,
        total_bytes: 100,
        stride: 0,
        capture_micros: 0,
        reference: None,
    };
    assert_eq!(
        d_zero_stride.fragment_count(),
        Err(WireError::InvalidFragment)
    );
}

#[test]
fn fragment_range_contiguous_and_exact_partitions() {
    let total = 2_550;
    let stride = 500;
    let d = FrameDescriptor {
        frame: 1,
        total_bytes: total,
        stride,
        capture_micros: 100,
        reference: None,
    };
    let count = d.fragment_count().unwrap();
    assert_eq!(count, 6);

    let mut cumulative_end = 0;
    for i in 0..count {
        let range = d.fragment_range(i).unwrap();
        assert_eq!(
            range.start, cumulative_end,
            "fragment {i} start must equal previous end"
        );
        if i == count - 1 {
            // Last fragment takes remainder: 2550 - 2500 = 50 bytes
            assert_eq!(range.end, total as usize);
            assert_eq!(range.len(), 50);
        } else {
            assert_eq!(range.len(), stride as usize);
        }
        cumulative_end = range.end;
    }
    assert_eq!(cumulative_end, total as usize);
}

#[test]
fn fragment_range_out_of_bounds_rejected() {
    let d = FrameDescriptor {
        frame: 1,
        total_bytes: 300,
        stride: 100,
        capture_micros: 100,
        reference: None,
    };
    assert_eq!(d.fragment_count().unwrap(), 3);

    // Valid indices: 0, 1, 2
    assert!(d.fragment_range(0).is_ok());
    assert!(d.fragment_range(1).is_ok());
    assert!(d.fragment_range(2).is_ok());

    // Out of bounds: index >= count
    assert_eq!(d.fragment_range(3), Err(WireError::InvalidFragment));
    assert_eq!(d.fragment_range(4), Err(WireError::InvalidFragment));
    assert_eq!(d.fragment_range(u32::MAX), Err(WireError::InvalidFragment));
}

#[test]
fn fragment_range_single_byte_and_stride_partitions() {
    let d = FrameDescriptor {
        frame: 0,
        total_bytes: 1,
        stride: 1_000,
        capture_micros: 0,
        reference: None,
    };
    assert_eq!(d.fragment_range(0), Ok(0..1));
    assert_eq!(d.fragment_range(1), Err(WireError::InvalidFragment));
}

#[test]
fn descriptor_validation_limits_and_dependencies() {
    let l = limits();

    // 1. Valid IDR (reference = None)
    let idr = FrameDescriptor {
        frame: 0,
        total_bytes: 1_000,
        stride: l.fragment_stride(),
        capture_micros: 0,
        reference: None,
    };
    assert!(idr.validate(&l).is_ok());

    // 2. Valid predicted frame (reference < frame)
    let pred = FrameDescriptor {
        frame: 5,
        total_bytes: 500,
        stride: l.fragment_stride(),
        capture_micros: 10,
        reference: Some(4),
    };
    assert!(pred.validate(&l).is_ok());

    // 3. Self-referential frame (reference == frame) is invalid
    let self_ref = FrameDescriptor {
        frame: 5,
        total_bytes: 500,
        stride: l.fragment_stride(),
        capture_micros: 10,
        reference: Some(5),
    };
    assert_eq!(self_ref.validate(&l), Err(WireError::InvalidDependency));

    // 4. Future reference (reference > frame) is invalid
    let future_ref = FrameDescriptor {
        frame: 5,
        total_bytes: 500,
        stride: l.fragment_stride(),
        capture_micros: 10,
        reference: Some(6),
    };
    assert_eq!(future_ref.validate(&l), Err(WireError::InvalidDependency));

    // 5. Zero total_bytes
    let zero_bytes = FrameDescriptor {
        total_bytes: 0,
        ..idr
    };
    assert_eq!(zero_bytes.validate(&l), Err(WireError::InvalidFragment));

    // 6. Zero stride
    let zero_stride = FrameDescriptor { stride: 0, ..idr };
    assert_eq!(zero_stride.validate(&l), Err(WireError::InvalidFragment));

    // 7. Stride exceeding limits
    let huge_stride = FrameDescriptor {
        stride: l.fragment_stride() + 1,
        ..idr
    };
    assert_eq!(huge_stride.validate(&l), Err(WireError::ResourceLimit));

    // 8. Total bytes exceeding protocol limit
    let huge_total = FrameDescriptor {
        total_bytes: l.protocol().max_encoded_access_unit_bytes() + 1,
        ..idr
    };
    assert_eq!(huge_total.validate(&l), Err(WireError::ResourceLimit));

    // 9. Fragment count exceeding max_fragments limit (16_384)
    let tiny_stride_exceeds_fragments = FrameDescriptor {
        total_bytes: 20_000,
        stride: 1, // 20_000 fragments > max_fragments (16_384)
        ..idr
    };
    assert_eq!(
        tiny_stride_exceeds_fragments.validate(&l),
        Err(WireError::ResourceLimit)
    );
}

#[test]
fn fragment_payload_mismatch_and_wire_roundtrip() {
    let l = limits();
    let d = FrameDescriptor {
        frame: 10,
        total_bytes: 250,
        stride: 100,
        capture_micros: 500,
        reference: Some(9),
    };
    assert!(d.validate(&l).is_ok());

    let payload = vec![0xAA; 100];
    let mut out = [0_u8; 256];

    // Correct payload length for index 0 (100 bytes)
    let n = encode_fragment(
        Fragment {
            descriptor: d,
            index: 0,
            bytes: &payload,
        },
        7,
        &l,
        &mut out,
    )
    .unwrap();

    let record = Record::decode(&out[..n], &l, 7, Channel::Video).unwrap();
    let decoded = decode_fragment(record, &l).unwrap();
    assert_eq!(decoded.descriptor, d);
    assert_eq!(decoded.index, 0);
    assert_eq!(decoded.bytes, &payload[..]);

    // Fragment 0 with incorrect payload size (99 bytes instead of 100)
    let short_payload = vec![0xAA; 99];
    assert_eq!(
        encode_fragment(
            Fragment {
                descriptor: d,
                index: 0,
                bytes: &short_payload,
            },
            7,
            &l,
            &mut out,
        ),
        Err(WireError::InvalidFragment)
    );

    // Fragment 0 with oversized payload (101 bytes instead of 100)
    let long_payload = vec![0xAA; 101];
    assert_eq!(
        encode_fragment(
            Fragment {
                descriptor: d,
                index: 0,
                bytes: &long_payload,
            },
            7,
            &l,
            &mut out,
        ),
        Err(WireError::InvalidFragment)
    );

    // Last fragment (index 2) expects exactly 50 bytes
    let last_payload = vec![0xBB; 50];
    let n2 = encode_fragment(
        Fragment {
            descriptor: d,
            index: 2,
            bytes: &last_payload,
        },
        7,
        &l,
        &mut out,
    )
    .unwrap();

    let record2 = Record::decode(&out[..n2], &l, 7, Channel::Video).unwrap();
    let decoded2 = decode_fragment(record2, &l).unwrap();
    assert_eq!(decoded2.index, 2);
    assert_eq!(decoded2.bytes, &last_payload[..]);
}
