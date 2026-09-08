use fr_core::limits::ProtocolLimits;
use fr_wire::stream::{RecordStream, StreamError};
use fr_wire::{
    Channel, FrameDescriptor, MediaLimits, PipelineState, Progress, Record, SourceObservation,
    WireError, decode_progress, encode_progress,
};
fn packet() -> Vec<u8> {
    let mut bytes = vec![0; 128];
    let n = encode_progress(
        Progress {
            descriptor: FrameDescriptor {
                frame: 7,
                total_bytes: 5,
                stride: 10,
                capture_micros: 9,
                reference: Some(6),
            },
            observed_micros: 10,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        17,
        &limits(),
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}
fn limits() -> MediaLimits {
    MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap()
}
fn stream() -> RecordStream {
    RecordStream::new(1150, 17, 5000).unwrap()
}
#[test]
fn every_possible_split_and_bytewise_delivery_preserve_real_records() {
    let p = packet();
    for split in 0..=p.len() {
        let mut s = stream();
        assert_eq!(s.push(&p[..split], 1).unwrap(), split);
        assert_eq!(s.push(&p[split..], 2).unwrap(), p.len() - split);
        let frame = s.frame(3).unwrap().unwrap();
        assert_eq!(frame, p);
        let record = Record::decode(frame, &limits(), 17, Channel::MediaConfig).unwrap();
        assert_eq!(
            decode_progress(record, &limits()).unwrap().descriptor.frame,
            7
        );
        s.consume(4).unwrap();
        assert_eq!(s.buffered_bytes(), 0);
        assert_eq!(s.next_deadline(), None);
    }
    let mut s = stream();
    for byte in &p {
        assert_eq!(s.push(&[*byte], 1), Ok(1));
    }
    assert_eq!(s.frame(2).unwrap(), Some(p.as_slice()));
}
#[test]
fn coalesced_records_stop_at_one_and_backpressure_never_discards_bytes() {
    let p = packet();
    let all = [p.as_slice(), p.as_slice(), p.as_slice()].concat();
    let mut s = stream();
    let mut at = 0;
    for now in 0..3 {
        assert_eq!(s.push(&all[at..], now), Ok(p.len()));
        at += p.len();
        assert_eq!(s.push(&all[at..], now), Ok(0));
        assert_eq!(s.frame(now).unwrap(), Some(p.as_slice()));
        assert!(s.allocated_bytes() <= 1150);
        s.consume(now).unwrap();
    }
    assert_eq!(at, all.len());
    s.finish(4).unwrap();
    assert_eq!(s.push(&p, 4), Err(StreamError::Closed));
}
#[test]
fn malformed_or_huge_header_fails_before_payload_allocation_and_never_resyncs() {
    let p = packet();
    for (offset, value, error) in [
        (0, 0, WireError::BadMagic),
        (5, 1, WireError::UnsupportedVersion),
        (9, 1, WireError::InvalidFlags),
        (19, 99, WireError::InvalidBinding),
        (12, 255, WireError::ResourceLimit),
        (20, 255, WireError::InvalidExtension),
    ] {
        let mut bad = p.clone();
        bad[offset] = value;
        let mut s = stream();
        assert_eq!(s.push(&bad[..24], 0), Err(StreamError::Wire(error)));
        assert_eq!(s.allocated_bytes(), 0);
        assert_eq!(s.push(&p, 1), Err(StreamError::Wire(error)));
    }
}
#[test]
fn partial_header_body_and_ready_record_have_nonrenewable_exclusive_deadlines() {
    let p = packet();
    for prefix in [1, 23, 24, p.len() - 1, p.len()] {
        let mut s = stream();
        s.push(&p[..prefix], 10).unwrap();
        assert_eq!(s.next_deadline(), Some(5010));
        s.push(&[], 5009).unwrap();
        assert_eq!(s.next_deadline(), Some(5010));
        assert_eq!(s.tick(5010), Err(StreamError::Expired));
        assert_eq!(s.allocated_bytes(), 0);
        assert_eq!(s.frame(5011), Err(StreamError::Expired));
    }
}
#[test]
fn eof_in_any_prefix_and_clock_faults_are_terminal() {
    let p = packet();
    for prefix in 1..p.len() {
        let mut s = stream();
        s.push(&p[..prefix], 0).unwrap();
        assert_eq!(s.finish(1), Err(StreamError::Wire(WireError::Truncated)));
        assert_eq!(s.allocated_bytes(), 0);
    }
    let mut s = stream();
    s.push(&p[..1], 50).unwrap();
    assert_eq!(s.push(&p[1..], 49), Err(StreamError::ClockRegression));
    let mut s = stream();
    assert_eq!(
        s.push(&p, u64::MAX),
        Err(StreamError::Wire(WireError::ArithmeticOverflow))
    );
    assert!(s.frame(u64::MAX).is_err());
}
#[test]
fn idle_stream_uses_no_heap_and_header_size_counts_toward_limit() {
    let mut s = stream();
    assert_eq!(s.push(&[], 0), Ok(0));
    assert_eq!(s.allocated_bytes(), 0);
    assert_eq!(s.next_deadline(), None);
    let p = packet();
    let mut exact = RecordStream::new(p.len(), 17, 50).unwrap();
    assert_eq!(exact.push(&p, 1), Ok(p.len()));
    let mut small = RecordStream::new(p.len() - 1, 17, 50).unwrap();
    assert_eq!(
        small.push(&p, 1),
        Err(StreamError::Wire(WireError::ResourceLimit))
    );
    for (size, binding, lifetime) in [
        (23, 1, 5),
        (65537, 1, 5),
        (24, 0, 5),
        (24, 1, 0),
        (24, 1, 5_000_001),
    ] {
        assert!(RecordStream::new(size, binding, lifetime).is_err());
    }
}
#[test]
fn diagnostic_formatting_is_independent_of_sensitive_record_bytes() {
    let p = packet();
    let mut q = p.clone();
    q[24..].fill(42);
    let mut a = stream();
    let mut b = stream();
    a.push(&p, 0).unwrap();
    b.push(&q, 0).unwrap();
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    assert_eq!(format!("{a:#?}"), format!("{b:#?}"));
}
