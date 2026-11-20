//! Exact private framing and original-deadline regressions; no device evidence.
use super::*;
fn options() -> CaptureOptions {
    #[cfg(target_os = "linux")]
    let selection = crate::audio_process::Selection::Pulse {
        server: "/tmp/private-pulse/native".into(),
        source: "explicit_mic".into(),
    };
    #[cfg(target_os = "macos")]
    let selection = crate::audio_process::Selection::Mac {
        uid: "AppleUSBAudioEngine:Selected:1".into(),
    };
    CaptureOptions {
        selection,
        configuration: AudioStreamConfig::new(
            AudioDirection::Uplink,
            AudioGeneration::from_raw(7),
            AudioChannels::Mono,
            if cfg!(target_os = "macos") { 10 } else { 20 },
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
    assert_eq!(decoded.selection, options.selection);
    assert_eq!(decoded.configuration, options.configuration);
    assert_eq!(decoded.bitrate, options.bitrate);
    assert_eq!(decoded.max_packet_bytes, options.max_packet_bytes);
    bytes[44] = 1;
    assert_eq!(
        Header::read(&mut bytes.as_slice(), false),
        Err(Error::Protocol)
    );
    #[cfg(target_os = "linux")]
    let invalid = ["", "@DEFAULT_SOURCE@", "source with space", "remote:source"];
    #[cfg(target_os = "macos")]
    let invalid = ["", "default", "system", " device"];
    for name in invalid {
        let mut options = options.clone();
        match &mut options.selection {
            crate::audio_process::Selection::Pulse { source, .. } => *source = name.into(),
            crate::audio_process::Selection::Mac { uid } => *uid = name.into(),
        }
        assert_eq!(configuration(&options), Err(Error::Selection));
    }
}
#[test]
fn private_source_discriminant_never_falls_back_to_another_platform() {
    let original = configuration(&options()).unwrap();
    assert_eq!(original[12], u8::from(cfg!(target_os = "macos")));
    for kind in [1 - original[12], 2, 255] {
        let mut changed = original.clone();
        changed[12] = kind;
        assert!(read_configuration(header(CONFIGURE), &changed).is_err());
    }
    let mut reserved = original;
    reserved[13] = 1;
    assert!(read_configuration(header(CONFIGURE), &reserved).is_err());
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
        options().configuration.frame_duration_ms() * 48,
        false,
        &[0xf8, 0xff, 0xfe],
    )
    .unwrap()
}
#[test]
fn capture_batches_require_exact_timeline_and_bounded_payloads() {
    let options = options();
    let second_timestamp = 1000 + u64::from(options.configuration.frame_duration_ms()) * 48;
    let units = [unit(7, 1000), unit(8, second_timestamp)];
    let valid = packets(&units, CaptureCounters::default(), &options).unwrap();
    let (decoded, counters) = read_packets(&valid, &options).unwrap();
    assert_eq!(counters, CaptureCounters::default());
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0].payload(), units[0].payload());
    assert_eq!(decoded[1].timestamp_samples(), second_timestamp);
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

#[test]
fn renewal_requires_the_original_serial_generation_and_unexpired_predecessor_bound() {
    let renewal = Header {
        kind: RENEW,
        serial: 8,
        authorization_ns: 20_000,
        expires_ns: 9_000,
        ..header(PULL)
    };
    let encoded = encoded_header(renewal, &[], false);
    assert_eq!(
        Header::read(&mut encoded.as_slice(), false).unwrap(),
        renewal
    );
    assert!(renewal.follows(8, 7, 10_000));
    for rejected in [
        Header {
            serial: 7,
            ..renewal
        },
        Header {
            generation: 8,
            ..renewal
        },
        Header {
            authorization_ns: 10_000,
            ..renewal
        },
        Header {
            expires_ns: 10_001,
            ..renewal
        },
        Header {
            bytes: 1,
            ..renewal
        },
        Header {
            kind: CONFIGURE,
            ..renewal
        },
    ] {
        assert!(!rejected.follows(8, 7, 10_000));
    }
    let tighter = Header {
        authorization_ns: 8_000,
        expires_ns: 7_000,
        ..renewal
    };
    assert!(tighter.follows(8, 7, 10_000));
    assert!(
        !Header {
            kind: PULL,
            ..renewal
        }
        .follows(8, 7, 10_000)
    );
    assert!(
        !Header {
            authorization_ns: 10_000,
            ..renewal
        }
        .matches_reply(renewal)
    );
}
