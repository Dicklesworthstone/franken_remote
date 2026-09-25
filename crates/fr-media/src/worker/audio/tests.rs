use super::*;
use crate::worker::{HEADER_BYTES, Header, Identity, Kind};
use fr_core::limits::ProtocolLimits;

fn capture() -> Capture {
    Capture {
        generation: AudioGeneration::from_raw(3),
        channels: AudioChannels::Stereo,
        frame_duration_ms: 20,
        bitrate: 96_000,
        max_packet_bytes: 1000,
        server: "/run/fr-audio/native".into(),
        monitor: Monitor::DefaultSink,
    }
}
fn unit(sequence: u64, timestamp: u64, len: usize) -> AudioAccessUnit {
    AudioAccessUnit::new(
        AudioDirection::Downlink,
        AudioGeneration::from_raw(3),
        sequence,
        timestamp,
        960,
        false,
        &vec![0x5a; len],
    )
    .unwrap()
}

#[test]
fn configuration_round_trips_and_refuses_hostile_or_unqualified_profiles() {
    for monitor in [Monitor::DefaultSink, Monitor::Sink("fr_host".into())] {
        let c = Capture {
            monitor,
            ..capture()
        };
        let body = c.encode().unwrap();
        assert!(Kind::ConfigureAudio.accepts_length(body.len(), &ProtocolLimits::ABSOLUTE));
        assert_eq!(Capture::decode(&body).unwrap(), c);
    }
    let bad = [
        Capture {
            generation: AudioGeneration::from_raw(0),
            ..capture()
        },
        Capture {
            frame_duration_ms: 5,
            ..capture()
        },
        Capture {
            frame_duration_ms: 40,
            ..capture()
        },
        Capture {
            max_packet_bytes: 1276,
            ..capture()
        },
        // 192 kbps cannot fit a 100-byte ceiling at 20 ms.
        Capture {
            bitrate: 192_000,
            max_packet_bytes: 100,
            ..capture()
        },
        Capture {
            server: "relative/native".into(),
            ..capture()
        },
        Capture {
            server: "unix:/run/native".into(),
            ..capture()
        },
        Capture {
            server: format!("/{}", "a".repeat(MAX_SERVER_BYTES)),
            ..capture()
        },
        Capture {
            monitor: Monitor::Sink("@DEFAULT_SINK@".into()),
            ..capture()
        },
        Capture {
            monitor: Monitor::Sink("bad sink".into()),
            ..capture()
        },
    ];
    for c in bad {
        assert!(c.encode().is_err(), "{c:?}");
    }
    // Forged lengths or reserved bytes never decode.
    let body = capture().encode().unwrap();
    for (at, value) in [(16, 0_u8), (16, 200), (17, 5), (18, 1)] {
        let mut forged = body.clone();
        forged[at] = value;
        assert!(Capture::decode(&forged).is_err(), "{at}");
    }
    for end in 0..body.len() {
        assert!(Capture::decode(&body[..end]).is_err());
    }
    // Debug never names the local server or device.
    let text = format!(
        "{:?}",
        Capture {
            monitor: Monitor::Sink("secret_sink".into()),
            ..capture()
        }
    );
    assert!(!text.contains("fr-audio") && !text.contains("secret_sink"));
}

#[test]
fn batch_round_trips_consecutive_packets_and_empty_pulls() {
    let c = capture();
    let packets = [unit(7, 9600, 240), unit(8, 10_560, 17)];
    let counters = CaptureCounters {
        gaps: 2,
        overruns: 1,
    };
    let body = encode_batch(&packets, counters, &c).unwrap();
    assert!(Kind::AudioPackets.accepts_length(body.len(), &ProtocolLimits::ABSOLUTE));
    let batch = decode_batch(&body, &c).unwrap();
    assert_eq!(batch.counters, counters);
    assert_eq!(batch.packets, packets);
    let empty = encode_batch(&[], CaptureCounters::default(), &c).unwrap();
    let batch = decode_batch(&empty, &c).unwrap();
    assert_eq!(batch.packets.len(), 0);
    // A source gap is legal; overlap or reordering is not.
    let gap = encode_batch(&[unit(7, 0, 5), unit(8, 5000, 5)], counters, &c).unwrap();
    assert_eq!(decode_batch(&gap, &c).unwrap().packets.len(), 2);
    for bad in [
        [unit(7, 0, 5), unit(9, 960, 5)],
        [unit(7, 960, 5), unit(8, 959, 5)],
        [unit(8, 0, 5), unit(7, 960, 5)],
    ] {
        let body = encode_batch(&bad, counters, &c).unwrap();
        assert_eq!(decode_batch(&body, &c).err(), Some(Error::Malformed));
    }
}

#[test]
fn hostile_batches_refuse_before_packet_allocation() {
    let c = capture();
    let body = encode_batch(&[unit(1, 0, 40)], CaptureCounters::default(), &c).unwrap();
    // Payload length forged above the configured ceiling, to zero, or past
    // the body; counts above the batch bound or disagreeing with the framing.
    for (at, bytes) in [
        (BATCH_HEADER_BYTES + 18, 1001_u16.to_be_bytes()),
        (BATCH_HEADER_BYTES + 18, 0_u16.to_be_bytes()),
        (BATCH_HEADER_BYTES + 18, 41_u16.to_be_bytes()),
        (BATCH_HEADER_BYTES + 18, 39_u16.to_be_bytes()),
        (BATCH_HEADER_BYTES + 16, 480_u16.to_be_bytes()),
    ] {
        let mut forged = body.clone();
        forged[at..at + 2].copy_from_slice(&bytes);
        assert!(decode_batch(&forged, &c).is_err(), "{at} {bytes:?}");
    }
    for count in [0_u8, 2, 9, 255] {
        let mut forged = body.clone();
        forged[0] = count;
        assert!(decode_batch(&forged, &c).is_err(), "{count}");
    }
    let mut reserved = body.clone();
    reserved[2] = 1;
    assert!(decode_batch(&reserved, &c).is_err());
    for end in 0..body.len() {
        assert!(decode_batch(&body[..end], &c).is_err());
    }
    // The private header refuses an oversized body before any read/allocation.
    let header = Header {
        kind: Kind::AudioPackets,
        identity: Identity {
            epoch: 1,
            sequence: 0,
        },
        length: MAX_BATCH_BYTES + 1,
    };
    assert_eq!(
        header.encode(&ProtocolLimits::ABSOLUTE),
        Err(Error::ResourceLimit)
    );
    let mut raw = Header {
        length: MAX_BATCH_BYTES,
        ..header
    }
    .encode(&ProtocolLimits::ABSOLUTE)
    .unwrap();
    raw[32..36].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(
        Header::decode(&raw, &ProtocolLimits::ABSOLUTE),
        Err(Error::ResourceLimit)
    );
    assert_eq!(raw.len(), HEADER_BYTES);
    // Packets from another generation, direction or duration never encode.
    let foreign = AudioAccessUnit::new(
        AudioDirection::Uplink,
        AudioGeneration::from_raw(3),
        1,
        0,
        960,
        false,
        &[1],
    )
    .unwrap();
    assert!(encode_batch(&[foreign], CaptureCounters::default(), &c).is_err());
    let nine: Vec<_> = (0..9).map(|n| unit(n, n * 960, 1)).collect();
    assert_eq!(
        encode_batch(&nine, CaptureCounters::default(), &c).err(),
        Some(Error::ResourceLimit)
    );
}
