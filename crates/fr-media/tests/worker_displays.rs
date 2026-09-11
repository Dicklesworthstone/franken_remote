use fr_core::{
    ids::{CodecConfigurationGeneration, DisplayGeometryGeneration},
    limits::ProtocolLimits,
};
use fr_media::worker::{
    self, Backend, Configuration, Error, Header, Identity, Kind, Record, capture::monitors::*,
};
use fr_wire::display::{Catalog, Display};
use std::io::Cursor;
fn config() -> Configuration {
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
fn catalog() -> Catalog {
    Catalog::new(
        2,
        &[Display {
            handle: 17,
            geometry: DisplayGeometryGeneration::INITIAL,
            x: -320,
            y: 0,
            pixel_width: 320,
            pixel_height: 240,
            logical_width: 320,
            logical_height: 240,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
        }],
        &ProtocolLimits::ABSOLUTE,
    )
    .unwrap()
}
#[test]
fn independent_catalog_bytes_preserve_signed_geometry_without_network_identity() {
    let bytes = encode_catalog(catalog(), &ProtocolLimits::ABSOLUTE).unwrap();
    let expected = [
        0, 0, 0, 0, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 17, 0, 0, 0, 0, 0,
        0, 0, 0, 255, 255, 254, 192, 0, 0, 0, 0, 0, 0, 1, 64, 0, 0, 0, 240, 0, 0, 1, 64, 0, 0, 0,
        240, 0, 0, 0, 1, 0, 0, 0, 1, 0,
    ];
    assert_eq!(bytes, expected);
    assert_eq!(
        decode_catalog(&expected, &ProtocolLimits::ABSOLUTE).unwrap(),
        catalog()
    );
}
#[test]
fn truncation_trailing_counts_duplicates_and_invalid_displays_fail() {
    let limits = ProtocolLimits::ABSOLUTE;
    let bytes = encode_catalog(catalog(), &limits).unwrap();
    for end in 0..bytes.len() {
        assert!(decode_catalog(&bytes[..end], &limits).is_err());
    }
    let mut bad = bytes.clone();
    bad.push(0);
    assert!(decode_catalog(&bad, &limits).is_err());
    bad = bytes.clone();
    bad[8] = 255;
    assert!(decode_catalog(&bad, &limits).is_err());
    bad = bytes.clone();
    bad.extend_from_slice(&bytes[9..]);
    bad[8] = 2;
    assert!(decode_catalog(&bad, &limits).is_err());
    bad = bytes;
    bad[9 + 32..9 + 36].fill(0);
    assert!(decode_catalog(&bad, &limits).is_err());
    let empty = Catalog::new(1, &[], &limits).unwrap();
    assert_eq!(
        decode_catalog(&encode_catalog(empty, &limits).unwrap(), &limits).unwrap(),
        empty
    );
}
#[test]
fn selected_configuration_pins_original_revision_handle_and_full_size() {
    let cat = catalog();
    let c = config();
    let selection = cat.selection(17).unwrap();
    let bytes = encode_configuration(c, selection, cat).unwrap();
    assert_eq!(bytes.len(), 52);
    assert_eq!(&bytes[..28], c.encode().unwrap());
    assert_eq!(&bytes[28..36], &2_u64.to_be_bytes());
    assert_eq!(&bytes[36..52], &17_u128.to_be_bytes());
    assert_eq!(
        decode_configuration(&bytes, cat).unwrap(),
        (c, cat.displays()[0])
    );
    for end in 0..bytes.len() {
        assert!(decode_configuration(&bytes[..end], cat).is_err());
    }
    let mut bad = bytes.clone();
    bad.push(0);
    assert!(decode_configuration(&bad, cat).is_err());
    bad = bytes.clone();
    bad[35] = 3;
    assert_eq!(decode_configuration(&bad, cat), Err(Error::GeometryChanged));
    bad = bytes;
    bad[51] = 99;
    assert_eq!(decode_configuration(&bad, cat), Err(Error::GeometryChanged));
    assert!(encode_configuration(Configuration { width: 640, ..c }, selection, cat).is_err());
    assert!(encode_configuration(c, fr_wire::display::Select { x: 1, ..selection }, cat).is_err());
}
#[test]
fn new_header_lengths_are_bounded_before_body_reads_and_roles_stay_distinct() {
    let limits = ProtocolLimits::ABSOLUTE;
    for (kind, length) in [
        (Kind::DiscoverMonitors, 0),
        (Kind::ConfigureMonitor, 52),
        (Kind::CheckMonitor, 0),
        (Kind::CaptureMonitors, 66),
        (Kind::MonitorReady, 52),
        (Kind::MonitorValid, 0),
    ] {
        let header = Header {
            kind,
            identity: Identity {
                epoch: 9,
                sequence: 0,
            },
            length,
        };
        let mut bytes = header.encode(&limits).unwrap();
        assert_eq!(Header::decode(&bytes, &limits).unwrap(), header);
        bytes[32..].copy_from_slice(&u32::MAX.to_be_bytes());
        let mut cursor = Cursor::new(bytes);
        assert_eq!(
            Record::read(&mut cursor, &limits).unwrap_err(),
            Error::ResourceLimit
        );
        assert_eq!(cursor.position(), worker::HEADER_BYTES as u64);
    }
    assert!(Kind::DiscoverMonitors.is_request());
    assert!(!Kind::CaptureMonitors.is_request());
    assert!(Kind::CheckMonitor.is_request());
    assert!(!Kind::MonitorValid.is_request());
}
