use super::*;
use asupersync::net::quic_core::{LongHeader, PacketHeader};

fn packet(
    kind: LongPacketType,
    version: u32,
    dcid: &[u8],
    scid: &[u8],
    token: &[u8],
    length: u64,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    PacketHeader::Long(LongHeader {
        packet_type: kind,
        version,
        dst_cid: ConnectionId::new(dcid).unwrap(),
        src_cid: ConnectionId::new(scid).unwrap(),
        token: token.to_vec(),
        payload_length: length.max(1),
        packet_number: 0,
        packet_number_len: 1,
    })
    .encode(&mut bytes)
    .unwrap();
    if length == 0 {
        let prefix = ProtectedHeaderPrefix::decode(&bytes, 0).unwrap();
        bytes[prefix.packet_number_offset().unwrap() - 1] = 0;
    }
    bytes.resize(1200, 0);
    bytes
}
fn valid() -> Vec<u8> {
    packet(
        LongPacketType::Initial,
        1,
        b"client-destination",
        b"client-source",
        b"",
        1100,
    )
}
#[test]
fn protected_bits_are_not_parsed_as_plaintext_packet_number_fields() {
    let mut bytes = valid();
    for masked in 0..16 {
        bytes[0] = 0xc0 | masked;
        assert_eq!(
            initial(&bytes),
            Some((
                ConnectionId::new(b"client-destination").unwrap(),
                ConnectionId::new(b"client-source").unwrap()
            ))
        );
    }
}
#[test]
fn truncated_oversized_and_wrong_packet_types_never_become_candidates() {
    let bytes = valid();
    for n in 0..1200 {
        assert!(initial(&bytes[..n]).is_none());
    }
    let mut oversized = bytes.clone();
    oversized.resize(1501, 0);
    assert!(initial(&oversized).is_none());
    for kind in [LongPacketType::Handshake, LongPacketType::ZeroRtt] {
        assert!(
            initial(&packet(
                kind,
                1,
                b"client-destination",
                b"client-source",
                b"",
                1100
            ))
            .is_none()
        );
    }
    for first in [0, 0x40, 0x80, 0xb0, 0xd0, 0xe0, 0xf0] {
        let mut wrong = bytes.clone();
        wrong[0] = first;
        assert!(initial(&wrong).is_none());
    }
}
#[test]
fn unsupported_version_token_or_cids_refuse_before_tls() {
    for version in [0, 2, u32::MAX] {
        let mut bytes = valid();
        bytes[1..5].copy_from_slice(&version.to_be_bytes());
        assert!(initial(&bytes).is_none());
    }
    for dcid in [b"".as_slice(), b"short"] {
        assert!(
            initial(&packet(
                LongPacketType::Initial,
                1,
                dcid,
                b"client-source",
                b"",
                1100
            ))
            .is_none()
        );
    }
    assert!(
        initial(&packet(
            LongPacketType::Initial,
            1,
            b"client-destination",
            b"",
            b"",
            1100
        ))
        .is_none()
    );
    assert!(
        initial(&packet(
            LongPacketType::Initial,
            1,
            b"client-destination",
            b"client-source",
            b"not-issued-here",
            1100
        ))
        .is_none()
    );
}
#[test]
fn declared_ciphertext_and_header_protection_sample_must_fit() {
    for length in [0, 1, 19, 1200, 1 << 40] {
        assert!(
            initial(&packet(
                LongPacketType::Initial,
                1,
                b"client-destination",
                b"client-source",
                b"",
                length
            ))
            .is_none()
        );
    }
    assert!(
        initial(&packet(
            LongPacketType::Initial,
            1,
            b"client-destination",
            b"client-source",
            b"",
            20
        ))
        .is_some()
    );
}
#[test]
fn configuration_is_bounded_and_matches_the_native_dial_profile() {
    let normal = Configuration::default();
    assert_eq!(normal.validate(), Ok(()));
    for count in [0, 257, u16::MAX] {
        assert_eq!(
            Configuration {
                max_initial_datagrams: count,
                ..normal
            }
            .validate(),
            Err(Error::Configuration)
        );
    }
    for duration in [Duration::ZERO, Duration::from_secs(31), Duration::MAX] {
        assert_eq!(
            Configuration {
                initial_timeout: duration,
                ..normal
            }
            .validate(),
            Err(Error::Configuration)
        );
        assert_eq!(
            Configuration {
                handshake_timeout: duration,
                ..normal
            }
            .validate(),
            Err(Error::Configuration)
        );
    }
    assert_eq!(
        Configuration {
            transport: Policy {
                stream_window: 131_072,
                ..normal.transport
            },
            ..normal
        }
        .validate(),
        Err(Error::Configuration)
    );
    assert_eq!(
        Configuration {
            transport: Policy {
                connection_window: 1_048_576,
                ..normal.transport
            },
            ..normal
        }
        .validate(),
        Err(Error::Configuration)
    );
    let parameters = TransportParameters::decode(&transport_parameters()).unwrap();
    assert_eq!(parameters.initial_max_data, Some(524_288));
    assert_eq!(parameters.initial_max_streams_uni, Some(8));
    assert_eq!(parameters.initial_max_streams_bidi, Some(0));
    assert!(parameters.disable_active_migration);
}
