//! Exact private framing and original-deadline regressions; no device evidence.
use super::*;
fn options() -> CaptureOptions {
    CaptureOptions {
        server: "/tmp/private-pulse/native".into(),
        source: "explicit_mic".into(),
        configuration: AudioStreamConfig::new(
            AudioDirection::Uplink,
            AudioGeneration::from_raw(7),
            AudioChannels::Mono,
            20,
            20,
        )
        .unwrap(),
        bitrate: 64_000,
        max_packet_bytes: 1000,
    }
}
fn header(kind: u8) -> Header {
    Header {
        kind,
        serial: 1,
        generation: 7,
        authorization_ns: 10_000,
        expires_ns: 5_000,
        bytes: 0,
        error: None,
    }
}
fn encoded_header(mut header: Header, body: &[u8], reply: bool) -> Vec<u8> {
    header.bytes = u32::try_from(body.len()).unwrap();
    let mut encoded = Vec::new();
    header.write(&mut encoded, reply, body).unwrap();
    encoded
}
#[test]
fn explicit_capture_selection_round_trips_without_an_implicit_alias() {
    let options = options();
    let body = configuration(&options).unwrap();
    let mut bytes = encoded_header(header(CONFIGURE), &body, false);
    let mut input = bytes.as_slice();
    let h = Header::read(&mut input, false).unwrap();
    let decoded = read_configuration(h, &super::body(&mut input, h).unwrap()).unwrap();
    assert_eq!(decoded.server, options.server);
    assert_eq!(decoded.source, options.source);
    assert_eq!(decoded.configuration, options.configuration);
    assert_eq!(decoded.bitrate, options.bitrate);
    assert_eq!(decoded.max_packet_bytes, options.max_packet_bytes);
    bytes[44] = 1;
    assert_eq!(
        Header::read(&mut bytes.as_slice(), false),
        Err(Error::Protocol)
    );
    for name in ["", "@DEFAULT_SOURCE@", "source with space", "remote:source"] {
        let mut options = options.clone();
        options.source = name.into();
        assert_eq!(configuration(&options), Err(Error::Selection));
    }
}
#[test]
fn oversized_or_misdirected_headers_are_refused_before_body_read() {
    let base = encoded_header(header(PULL), &[], false);
    for (offset, value) in [(4, 2), (5, 0), (6, 1), (7, 1), (44, 1), (47, 1)] {
        let mut bytes = base.clone();
        bytes[offset] = value;
        assert_eq!(
            Header::read(&mut bytes.as_slice(), false),
            Err(Error::Protocol)
        );
    }
    for size in [1, u32::MAX] {
        let mut bytes = base.clone();
        bytes[40..44].copy_from_slice(&size.to_be_bytes());
        assert_eq!(
            Header::read(&mut bytes.as_slice(), false),
            Err(Error::Protocol)
        );
    }
    let mut bytes = base;
    bytes[32..40].copy_from_slice(&10_001_u64.to_be_bytes());
    assert_eq!(
        Header::read(&mut bytes.as_slice(), false),
        Err(Error::Protocol)
    );
}
#[test]
fn only_the_original_exchange_can_reply_and_expiry_can_only_shorten() {
    let request = header(PULL);
    assert!(request.matches_reply(request));
    assert!(
        Header {
            expires_ns: request.expires_ns - 1,
            ..request
        }
        .matches_reply(request)
    );
    for other in [
        Header {
            expires_ns: request.expires_ns + 1,
            ..request
        },
        Header {
            authorization_ns: request.authorization_ns + 1,
            ..request
        },
        Header {
            generation: request.generation + 1,
            ..request
        },
        Header {
            serial: request.serial + 1,
            ..request
        },
        Header {
            kind: CONFIGURE,
            ..request
        },
    ] {
        assert!(!other.matches_reply(request));
    }
}
fn unit(sequence: u64, timestamp: u64) -> AudioAccessUnit {
    AudioAccessUnit::new(
        AudioDirection::Uplink,
        AudioGeneration::from_raw(7),
        sequence,
        timestamp,
        960,
        false,
        &[0xf8, 0xff, 0xfe],
    )
    .unwrap()
}
#[test]
fn capture_batches_require_exact_timeline_and_bounded_payloads() {
    let options = options();
    let units = [unit(7, 1000), unit(8, 1960)];
    let valid = packets(&units, CaptureCounters::default(), &options).unwrap();
    let (decoded, counters) = read_packets(&valid, &options).unwrap();
    assert_eq!(counters, CaptureCounters::default());
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0].payload(), units[0].payload());
    assert_eq!(decoded[1].timestamp_samples(), 1960);
    let second = BATCH_BYTES + PACKET_BYTES + 3;
    for (offset, value) in [
        (0, 5),
        (1, 1),
        (BATCH_BYTES + 18, 255),
        (second + 7, 7),
        (second + 15, 169),
    ] {
        let mut bytes = valid.clone();
        bytes[offset] = value;
        assert!(read_packets(&bytes, &options).is_err());
    }
    let mut trailing = valid.clone();
    trailing.push(0);
    assert!(read_packets(&trailing, &options).is_err());
    for n in 0..valid.len() {
        assert!(read_packets(&valid[..n], &options).is_err());
    }
    let mut gap = valid;
    gap[7] = 1;
    assert!(matches!(
        read_packets(&gap, &options),
        Err(Error::CaptureDiscontinuity)
    ));
}
