//! Display-space placement and private IPC; no native presentation evidence.
use fr_core::{
    ids::CodecConfigurationGeneration,
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_media::worker::{
    Backend, Configuration, Error, Header, Identity, Kind, Record,
    presentation::{Fit, MAPPED_TARGET_BYTES, MappedX11Target, X11Target},
};

fn configuration(width: u32, height: u32) -> Configuration {
    Configuration {
        width,
        height,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: 1_048_576,
        generation: CodecConfigurationGeneration::INITIAL,
    }
}

fn mapping(width: u32, height: u32, target_width: u32, target_height: u32) -> MappedX11Target {
    MappedX11Target::new(
        X11Target::new(19, target_width, target_height).unwrap(),
        width,
        height,
    )
    .unwrap()
}

fn body(configuration: Configuration, target: MappedX11Target) -> Vec<u8> {
    let mut body = configuration.encode().unwrap();
    // A length/framing fixture only; native decode still validates actual hvcC.
    body.extend_from_slice(&[0; 23]);
    let display = target.display_size();
    for value in [
        target.target().window(),
        target.target().width(),
        target.target().height(),
        display.0,
        display.1,
    ] {
        body.extend_from_slice(&value.to_be_bytes());
    }
    body
}

#[test]
fn canonical_even_crop_keeps_the_original_display_rectangle() {
    let target = mapping(5120, 2880, 1000, 750);
    assert_eq!(
        target.fit(),
        Fit {
            x: 0,
            y: 94,
            width: 1000,
            height: 562,
        }
    );
    assert_eq!(
        target.validate_picture(1000, 562, &ProtocolLimits::ABSOLUTE),
        Ok(())
    );
    assert_eq!(
        Fit::new(1000, 562, target.target()),
        Err(Error::GeometryChanged),
        "the encoded crop cannot redefine the original target's layout"
    );

    let target = mapping(5120, 2880, 1582, 888);
    assert_eq!(
        target.fit(),
        Fit {
            x: 2,
            y: 0,
            width: 1578,
            height: 888,
        }
    );
    assert_eq!(
        target.validate_picture(1582, 888, &ProtocolLimits::ABSOLUTE),
        Ok(())
    );
    // A shared source may retain another viewer's smaller canonical picture.
    // That affects sampling, never the display-space rectangle or its bars.
    let fit = target.fit();
    for picture in [(5120, 2880), (1920, 1080), (1000, 562), (640, 360)] {
        assert_eq!(
            target.validate_picture(picture.0, picture.1, &ProtocolLimits::ABSOLUTE),
            Ok(())
        );
        assert_eq!(target.fit(), fit);
    }
}

#[test]
fn odd_original_display_is_valid_without_relaxing_legacy_fit_or_hevc_crops() {
    let target = mapping(1365, 767, 1000, 750);
    assert_eq!(target.display_size(), (1365, 767));
    assert_eq!(
        target.validate_picture(1000, 560, &ProtocolLimits::ABSOLUTE),
        Ok(())
    );
    assert_eq!(
        Fit::new(1365, 767, target.target()),
        Err(Error::GeometryChanged)
    );
    assert_eq!(
        target.validate_picture(1365, 767, &ProtocolLimits::ABSOLUTE),
        Err(Error::GeometryChanged)
    );
    assert_eq!(
        target.validate_picture(1000, 562, &ProtocolLimits::ABSOLUTE),
        Err(Error::GeometryChanged)
    );
}

#[test]
fn mapped_configuration_roundtrips_its_exact_target_and_original_display() {
    let target = mapping(5120, 2880, 1000, 750);
    let config = configuration(1000, 562);
    let data = body(config, target);
    let (decoded, record, received) = MappedX11Target::decode_decoder(&data).unwrap();
    assert_eq!(decoded, config);
    assert_eq!(record, &[0; 23]);
    assert_eq!(received, target);
    assert_eq!(received.fit(), target.fit());
    let identity = Identity {
        epoch: 41,
        sequence: 7,
    };
    for (kind, code) in [
        (Kind::ConfigureMappedPresentation, 22_u16),
        (Kind::MappedPresentationReady, 282),
    ] {
        let header = Header {
            kind,
            identity,
            length: data.len(),
        };
        let bytes = header.encode(&ProtocolLimits::ABSOLUTE).unwrap();
        assert_eq!(&bytes[6..8], &code.to_be_bytes());
        assert_eq!(
            Header::decode(&bytes, &ProtocolLimits::ABSOLUTE),
            Ok(header)
        );
        assert_eq!(kind.is_request(), code < 256);
    }
    assert!(X11Target::decode_decoder(&data).is_err());
    assert!(X11Target::decode_fitted_decoder(&data).is_err());
}

#[test]
fn truncated_invalid_and_noncanonical_mappings_refuse_before_native_work() {
    let target = mapping(5120, 2880, 1000, 750);
    let data = body(configuration(1000, 562), target);
    for length in 0..data.len() {
        assert!(MappedX11Target::decode_decoder(&data[..length]).is_err());
    }
    let offset = data.len() - MAPPED_TARGET_BYTES;
    for (field, value) in [
        (0, 0),
        (4, 1001),
        (4, 5122),
        (8, 0),
        (12, 0),
        (12, u32::MAX),
        (16, 8192),
        (16, 2870),
    ] {
        let mut altered = data.clone();
        altered[offset + field..offset + field + 4].copy_from_slice(&value.to_be_bytes());
        assert!(
            MappedX11Target::decode_decoder(&altered).is_err(),
            "field {field} value {value}"
        );
    }
    for picture in [(1000, 560), (1000, 564), (1002, 560), (5120, 2878)] {
        assert_eq!(
            MappedX11Target::decode_decoder(&body(configuration(picture.0, picture.1), target,)),
            Err(Error::GeometryChanged)
        );
    }
}

#[test]
fn original_display_and_allocated_picture_have_separate_bounded_scopes() {
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_dimension_pixels: Some(1920),
        max_coded_pixels: Some(1920 * 1080),
        ..LimitOverrides::default()
    })
    .unwrap();
    let target = mapping(5120, 2880, 1000, 750);
    assert_eq!(target.validate_picture(1000, 562, &limits), Ok(()));
    assert_eq!(
        target.validate_picture(5120, 2880, &limits),
        Err(Error::ResourceLimit)
    );
    assert_eq!(
        mapping(5120, 2880, 2000, 1000).validate_picture(1000, 562, &limits),
        Err(Error::ResourceLimit)
    );
    assert!(MappedX11Target::new(X11Target::new(1, 16, 16).unwrap(), 8192, 8192).is_err());
    assert!(MappedX11Target::new(X11Target::new(1, 1000, 750).unwrap(), 998, 750).is_err());
    let pixels = ProtocolLimits::with_overrides(LimitOverrides {
        max_coded_pixels: Some(500_000),
        ..LimitOverrides::default()
    })
    .unwrap();
    assert_eq!(
        target.validate_picture(640, 360, &pixels),
        Err(Error::ResourceLimit),
        "small decoded pictures do not bypass the destination allocation limit"
    );
}

#[test]
fn mapped_header_length_and_epoch_are_checked_before_reading_payload() {
    let minimum = 28 + 23 + MAPPED_TARGET_BYTES;
    let header = Header {
        kind: Kind::ConfigureMappedPresentation,
        identity: Identity {
            epoch: 1,
            sequence: 0,
        },
        length: minimum,
    };
    for length in [0, MAPPED_TARGET_BYTES, minimum - 1, usize::MAX] {
        assert_eq!(
            Header { length, ..header }.encode(&ProtocolLimits::ABSOLUTE),
            Err(Error::ResourceLimit)
        );
    }
    let mut bytes = header.encode(&ProtocolLimits::ABSOLUTE).unwrap();
    bytes[32..36].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(
        Record::read(&mut std::io::Cursor::new(bytes), &ProtocolLimits::ABSOLUTE).unwrap_err(),
        Error::ResourceLimit
    );
    assert_eq!(
        Header {
            identity: Identity {
                epoch: 0,
                ..header.identity
            },
            ..header
        }
        .encode(&ProtocolLimits::ABSOLUTE),
        Err(Error::WrongEpoch)
    );
}
