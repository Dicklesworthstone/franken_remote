//! Local layout/IPC contracts, not decoder execution or visible-pixel evidence.
use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::worker::{
    Backend, Configuration, Error, Header, Identity, Kind,
    presentation::{Fit, TARGET_BYTES, X11Target},
};
fn config() -> Configuration {
    Configuration {
        width: 1920,
        height: 1080,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: 1_048_576,
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
fn body(target: X11Target) -> Vec<u8> {
    let mut body = config().encode().unwrap();
    body.extend_from_slice(&[0; 23]); // Length fixture only; real decoder still validates hvcC.
    for value in [target.window(), target.width(), target.height()] {
        body.extend_from_slice(&value.to_be_bytes());
    }
    body
}
#[test]
fn scaling_is_a_distinct_local_protocol_and_cannot_weaken_native_pixel_admission() {
    let target = X11Target::new(19, 960, 540).unwrap();
    let data = body(target);
    assert_eq!(
        X11Target::decode_decoder(&data),
        Err(Error::GeometryChanged)
    );
    let (decoded, record, received) = X11Target::decode_fitted_decoder(&data).unwrap();
    assert_eq!(decoded, config());
    assert_eq!(record, &[0; 23]);
    assert_eq!(received, target);
    let identity = Identity {
        epoch: 1,
        sequence: 2,
    };
    for (kind, number) in [
        (Kind::ConfigureFittedPresentation, 15_u16),
        (Kind::FittedPresentationReady, 273),
    ] {
        let header = Header {
            kind,
            identity,
            length: data.len(),
        };
        let bytes = header.encode(&ProtocolLimits::ABSOLUTE).unwrap();
        assert_eq!(&bytes[6..8], &number.to_be_bytes());
        assert_eq!(
            Header::decode(&bytes, &ProtocolLimits::ABSOLUTE),
            Ok(header)
        );
    }
}
#[test]
fn fitted_decode_refuses_truncation_zero_window_and_upscale() {
    let data = body(X11Target::new(19, 960, 540).unwrap());
    for n in 0..data.len() {
        assert!(X11Target::decode_fitted_decoder(&data[..n]).is_err());
    }
    let mut data = data;
    let offset = data.len() - TARGET_BYTES;
    data[offset..offset + 4].copy_from_slice(&0_u32.to_be_bytes());
    assert_eq!(
        X11Target::decode_fitted_decoder(&data),
        Err(Error::Malformed)
    );
    for dimensions in [(1922, 1080), (1920, 1082), (2048, 2048)] {
        assert!(
            X11Target::decode_fitted_decoder(&body(
                X11Target::new(19, dimensions.0, dimensions.1).unwrap()
            ))
            .is_err()
        );
    }
}
#[test]
fn aspect_fit_is_centered_and_keeps_odd_remainders_outside_the_image() {
    let fit = |w, h, tw, th| Fit::new(w, h, X11Target::new(1, tw, th).unwrap()).unwrap();
    assert_eq!(
        fit(1920, 1080, 960, 540),
        Fit {
            x: 0,
            y: 0,
            width: 960,
            height: 540
        }
    );
    assert_eq!(
        fit(1920, 1080, 800, 600),
        Fit {
            x: 0,
            y: 75,
            width: 800,
            height: 450
        }
    );
    assert_eq!(
        fit(1080, 1920, 600, 800),
        Fit {
            x: 75,
            y: 0,
            width: 450,
            height: 800
        }
    );
    assert_eq!(
        fit(1920, 1080, 642, 480),
        Fit {
            x: 0,
            y: 59,
            width: 642,
            height: 361
        }
    );
    for (w, h) in [(0, 100), (100, 0), (15, 32), (100, 101), (u32::MAX, 32)] {
        assert!(Fit::new(w, h, X11Target::new(1, 16, 16).unwrap()).is_err());
    }
}
