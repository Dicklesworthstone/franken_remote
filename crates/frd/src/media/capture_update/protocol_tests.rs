use super::*;
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::access_unit::FrameKind;

fn request(bitrate: Option<u32>) -> Request {
    Request {
        configuration: Configuration {
            width: 320,
            height: 240,
            fps: 30,
            backend: worker::Backend::SoftwareExplicit,
            bitrate: 8_000_000,
            max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
            generation: CodecConfigurationGeneration::INITIAL,
        },
        frame: FrameId::from_raw(2),
        observed_micros: 120_000,
        force_idr: false,
        conditional: true,
        bitrate,
        reference: Some(FrameId::from_raw(1)),
    }
}

fn unit(request: &Request, actual: Option<u32>, idr: bool) -> Vec<u8> {
    let mut body = worker::unit_parts(
        request.frame,
        request.observed_micros,
        request.configuration.generation,
        if idr {
            FrameKind::Idr {
                recovery: RecoveryGeneration::INITIAL,
            }
        } else {
            FrameKind::Predicted {
                references: request.reference.unwrap(),
            }
        },
        &[7; 16],
    )
    .unwrap();
    if let Some(actual) = actual {
        body[33..37].copy_from_slice(&actual.to_be_bytes());
    }
    body
}

fn unchanged(request: &Request, actual: Option<u32>) -> Vec<u8> {
    let proof = UnchangedCapture {
        candidate: request.frame,
        reference: request.reference.unwrap(),
        observed_micros: request.observed_micros,
    };
    actual.map_or_else(
        || proof.encode().unwrap(),
        |rate| worker::rate::unchanged_payload(proof, rate).unwrap(),
    )
}

#[test]
fn request_encoding_preserves_candidate_time_and_all_force_conditional_modes() {
    for bitrate in [None, Some(4_000_000)] {
        for conditional in [false, true] {
            for force_idr in [false, true] {
                let mut request = request(bitrate);
                request.conditional = conditional;
                request.force_idr = force_idr;
                let (kind, body) = request.payload().unwrap();
                if let Some(bitrate) = bitrate {
                    assert_eq!(kind, Kind::CaptureAtRate);
                    let decoded = worker::rate::Capture::decode(&body).unwrap();
                    assert_eq!(decoded.frame, request.frame);
                    assert_eq!(decoded.observed_micros, request.observed_micros);
                    assert_eq!(decoded.force_idr, force_idr);
                    assert_eq!(decoded.only_if_changed, conditional);
                    assert_eq!(decoded.bitrate, bitrate);
                } else {
                    assert_eq!(
                        kind,
                        if conditional {
                            Kind::CaptureIfChanged
                        } else {
                            Kind::Capture
                        }
                    );
                    assert_eq!(
                        worker::parse_capture(&body).unwrap(),
                        (request.frame, request.observed_micros, force_idr)
                    );
                }
            }
        }
    }
}

#[test]
fn rate_preflight_preserves_original_cap_and_software_only_admission() {
    let mut configuration = request(None).configuration;
    for rate in [10_000, 4_000_000, 8_000_000] {
        assert_eq!(validate_bitrate(configuration, Some(rate)), Ok(()));
    }
    for rate in [0, 9_999, 8_000_001, u32::MAX] {
        assert_eq!(
            validate_bitrate(configuration, Some(rate)),
            Err(worker::Error::ResourceLimit)
        );
    }
    for backend in [
        worker::Backend::Nvenc,
        worker::Backend::Vaapi,
        worker::Backend::VideoToolbox,
    ] {
        configuration.backend = backend;
        assert_eq!(
            validate_bitrate(configuration, Some(4_000_000)),
            Err(worker::Error::Unsupported)
        );
        assert_eq!(validate_bitrate(configuration, None), Ok(()));
    }
}

#[test]
fn encoded_reply_keeps_actual_rate_and_reuses_the_original_bounded_allocation() {
    for (kind, requested, actual) in [
        (Kind::Unit, None, None),
        (Kind::RateUnit, Some(4_000_000), Some(8_000_000)),
        (Kind::RateUnit, Some(4_000_000), Some(4_000_000)),
    ] {
        let request = request(requested);
        let body = unit(&request, actual, true);
        let allocation = body.as_ptr();
        let (content, applied) = request.response(kind, body).unwrap().unwrap();
        assert_eq!(
            applied, actual,
            "requested rate must never replace actual output"
        );
        let Content::Encoded(unit) = content else {
            panic!("encoded reply")
        };
        assert_eq!(unit.frame(), request.frame);
        assert_eq!(unit.capture_micros(), request.observed_micros);
        assert_eq!(unit.config_generation(), request.configuration.generation);
        assert_eq!(unit.bytes(), &[7; 16]);
        assert_eq!(unit.bytes().as_ptr(), allocation);
    }
}

#[test]
fn unchanged_reply_preserves_actual_rate_and_original_observation_identity() {
    for (kind, requested, actual) in [
        (Kind::Unchanged, None, None),
        (Kind::RateUnchanged, Some(4_000_000), Some(8_000_000)),
        (Kind::RateUnchanged, Some(4_000_000), Some(4_000_000)),
    ] {
        let request = request(requested);
        let (content, applied) = request
            .response(kind, unchanged(&request, actual))
            .unwrap()
            .unwrap();
        assert_eq!(applied, actual);
        let Content::Unchanged(proof) = content else {
            panic!("unchanged reply")
        };
        assert_eq!(proof.candidate, request.frame);
        assert_eq!(Some(proof.reference), request.reference);
        assert_eq!(proof.observed_micros, request.observed_micros);
    }
}

#[test]
fn legacy_and_rate_reply_modes_cannot_be_interchanged() {
    for (request, kinds) in [
        (request(None), [Kind::RateUnit, Kind::RateUnchanged]),
        (request(Some(4_000_000)), [Kind::Unit, Kind::Unchanged]),
    ] {
        for kind in kinds {
            assert_eq!(
                request.response(kind, vec![]).unwrap_err(),
                Error::InvalidFrame,
                "mode mismatch is rejected before parsing the other mode's payload"
            );
        }
    }
}

#[test]
fn every_encoded_reply_must_match_original_candidate_configuration_and_time() {
    for (kind, rate) in [(Kind::Unit, None), (Kind::RateUnit, Some(4_000_000))] {
        let request = request(rate);
        for offset in [0, 8, 16] {
            let mut body = unit(&request, rate, true);
            let old = u64::from_be_bytes(body[offset..offset + 8].try_into().unwrap());
            body[offset..offset + 8].copy_from_slice(&(old + 1).to_be_bytes());
            assert_eq!(
                request.response(kind, body).unwrap_err(),
                Error::InvalidFrame,
                "identity field at {offset} must remain exact"
            );
        }
    }
}

#[test]
fn forced_idr_and_unconditional_captures_cannot_accept_unchanged_replies() {
    for rate in [None, Some(4_000_000)] {
        let mut request = request(rate);
        let unit_kind = if rate.is_some() {
            Kind::RateUnit
        } else {
            Kind::Unit
        };
        let unchanged_kind = if rate.is_some() {
            Kind::RateUnchanged
        } else {
            Kind::Unchanged
        };
        request.force_idr = true;
        assert_eq!(
            request
                .response(unit_kind, unit(&request, rate, false))
                .unwrap_err(),
            Error::InvalidFrame
        );
        assert_eq!(
            request
                .response(unchanged_kind, unchanged(&request, rate))
                .unwrap_err(),
            Error::Backpressure
        );
        assert!(
            request
                .response(unit_kind, unit(&request, rate, true))
                .is_ok()
        );
        request.force_idr = false;
        request.conditional = false;
        assert_eq!(
            request
                .response(unchanged_kind, unchanged(&request, rate))
                .unwrap_err(),
            Error::Backpressure
        );
        assert!(
            request
                .response(unit_kind, unit(&request, rate, false))
                .is_ok()
        );
    }
}

#[test]
fn unchanged_reply_requires_exact_candidate_reference_time_and_a_source_anchor() {
    for (kind, rate) in [
        (Kind::Unchanged, None),
        (Kind::RateUnchanged, Some(4_000_000)),
    ] {
        let mut request = request(rate);
        for (offset, value) in [(0, 3_u64), (8, 0), (16, request.observed_micros + 1)] {
            let mut body = unchanged(&request, rate);
            body[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
            assert_eq!(
                request.response(kind, body).unwrap_err(),
                Error::InvalidFrame
            );
        }
        let body = unchanged(&request, rate);
        request.reference = None;
        assert_eq!(
            request.response(kind, body).unwrap_err(),
            Error::InvalidFrame
        );
    }
}

#[test]
fn actual_reply_rates_remain_bounded_by_the_original_configuration() {
    let request = request(Some(4_000_000));
    for (kind, body) in [
        (Kind::RateUnit, unit(&request, Some(8_000_001), true)),
        (Kind::RateUnchanged, unchanged(&request, Some(8_000_001))),
    ] {
        assert_eq!(
            request.response(kind, body).unwrap_err(),
            Error::InvalidFrame
        );
    }
    for (kind, mut body, offset) in [
        (Kind::RateUnit, unit(&request, Some(4_000_000), true), 33),
        (
            Kind::RateUnchanged,
            unchanged(&request, Some(4_000_000)),
            24,
        ),
    ] {
        body[offset..offset + 4].fill(0);
        assert_eq!(
            request.response(kind, body).unwrap_err(),
            Error::Protocol(worker::Error::ResourceLimit)
        );
    }
}

#[test]
fn strict_reserved_bytes_lengths_and_access_unit_limits_remain_enforced() {
    for (kind, rate, reserved) in [
        (Kind::Unit, None, 33),
        (Kind::RateUnit, Some(4_000_000), 37),
    ] {
        let mut request = request(rate);
        let mut body = unit(&request, rate, true);
        body[reserved] = 1;
        assert_eq!(
            request.response(kind, body).unwrap_err(),
            Error::Protocol(worker::Error::Malformed)
        );
        let body = unit(&request, rate, true);
        request.configuration.max_access_unit_bytes = 8;
        assert_eq!(
            request.response(kind, body).unwrap_err(),
            Error::Protocol(worker::Error::ResourceLimit)
        );
    }
    for (kind, rate) in [
        (Kind::Unchanged, None),
        (Kind::RateUnchanged, Some(4_000_000)),
    ] {
        let request = request(rate);
        let mut body = unchanged(&request, rate);
        body.push(0);
        assert_eq!(
            request.response(kind, body).unwrap_err(),
            Error::Protocol(worker::Error::Malformed)
        );
    }
}

#[test]
fn polling_does_not_rebase_the_original_request_or_accept_unexpected_responses() {
    let request = request(Some(4_000_000));
    assert!(request.response(Kind::NeedInput, vec![]).unwrap().is_none());
    for kind in [
        Kind::NeedDrain,
        Kind::Ready,
        Kind::Stopped,
        Kind::CursorSnapshot,
    ] {
        assert_eq!(
            request.response(kind, vec![]).unwrap_err(),
            Error::Backpressure
        );
    }
    let mut body = unit(&request, Some(4_000_000), true);
    body[8..16].copy_from_slice(&(request.observed_micros + 1).to_be_bytes());
    assert_eq!(
        request.response(Kind::RateUnit, body).unwrap_err(),
        Error::InvalidFrame
    );
    assert!(
        request
            .response(Kind::RateUnit, unit(&request, Some(4_000_000), true))
            .is_ok()
    );
}
