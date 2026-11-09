//! Host normalization uses the retained real `VideoToolbox` fixtures, not a
//! substitute encoder. These are software admission tests, not native timing.
use super::{VT_IDR, VT_P1, VT_P2, VT_PPS, VT_SPS, VT_VPS, au, hex, vt_config};
use crate::hevc::{HevcError, HevcGuard, framing, strip_encoder_sei};
use fr_core::limits::{LimitOverrides, ProtocolLimits};

fn native(nals: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for nal in nals {
        bytes.extend_from_slice(&u32::try_from(nal.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(nal);
    }
    bytes
}

fn startup() -> Vec<Vec<u8>> {
    [VT_VPS, VT_SPS, VT_PPS, VT_IDR]
        .into_iter()
        .map(hex)
        .collect()
}

fn previous_path(nals: &[Vec<u8>]) -> Vec<u8> {
    let refs: Vec<_> = nals.iter().map(Vec::as_slice).collect();
    let stripped = strip_encoder_sei(&au(&refs)).unwrap();
    framing::annex_b_to_length_prefixed(&stripped, ProtocolLimits::ABSOLUTE).unwrap()
}

fn guard() -> HevcGuard {
    HevcGuard::new(vt_config(), ProtocolLimits::ABSOLUTE, 4).unwrap()
}

#[test]
fn actual_videotoolbox_idr_and_reference_chain_match_previous_output() {
    let mut encoder = guard();
    let mut viewer = guard();
    for (nals, idr) in [
        (startup(), true),
        (vec![hex(VT_P1)], false),
        (vec![hex(VT_P2)], false),
    ] {
        let expected = previous_path(&nals);
        // Host timing SEI is removed per NAL, including metadata before or
        // after pictures. It is never accepted on a viewer input path.
        let mut with_sei = nals;
        with_sei.insert(0, vec![78, 1, 1, 0, 128]);
        with_sei.push(vec![80, 1, 1, 0, 128]);
        let normalized = encoder
            .normalize_encoder_length_prefixed(&native(&with_sei), idr)
            .unwrap();
        assert_eq!(normalized, expected);
        assert_eq!(normalized, previous_path(&with_sei));
        assert_eq!(
            viewer
                .validate_length_prefixed(&normalized, idr)
                .unwrap()
                .idr,
            idr
        );
    }
    assert_eq!(
        encoder.decoder_record().unwrap().bytes(),
        viewer.decoder_record().unwrap().bytes()
    );
}

#[test]
fn native_trailing_zero_padding_keeps_the_previous_normalization() {
    let mut nals = startup();
    for nal in &mut nals {
        nal.extend_from_slice(&[0, 0, 0, 0]);
    }
    let normalized = guard()
        .normalize_encoder_length_prefixed(&native(&nals), true)
        .unwrap();
    assert_eq!(normalized, previous_path(&nals));
    assert_eq!(normalized, native(&startup()));
}

#[test]
fn malformed_native_framing_is_refused_before_parameter_state_changes() {
    for bytes in [
        vec![],
        vec![0, 0, 0],
        vec![0, 0, 0, 0],
        vec![0, 0, 0, 2, 40, 1],
        vec![255, 255, 255, 255, 40, 1, 128],
        vec![0, 0, 0, 4, 40, 1, 128],
        // Removing zero padding must not turn a header-only NAL into data.
        native(&[vec![78, 1, 0, 0]]),
    ] {
        let mut encoder = guard();
        assert!(
            encoder
                .normalize_encoder_length_prefixed(&bytes, true)
                .is_err()
        );
        assert!(matches!(
            encoder.decoder_record(),
            Err(HevcError::MissingParameterSet)
        ));
        encoder
            .normalize_encoder_length_prefixed(&native(&startup()), true)
            .unwrap();
    }
}

#[test]
fn complete_retained_payload_and_reference_history_still_require_admission() {
    let mut encoder = guard();
    encoder
        .normalize_encoder_length_prefixed(&native(&startup()), true)
        .unwrap();
    for (suffix, error) in [
        (&[0, 0, 1, 0x42, 1][..], HevcError::EmulationPrevention),
        (&[0, 0, 2], HevcError::EmulationPrevention),
        (&[0, 0, 3], HevcError::Truncated),
        (&[0, 0, 3, 4], HevcError::EmulationPrevention),
    ] {
        let mut picture = hex(VT_P1);
        picture.extend_from_slice(suffix);
        assert_eq!(
            encoder.normalize_encoder_length_prefixed(&native(&[picture]), false),
            Err(error)
        );
    }
    assert_eq!(
        encoder.normalize_encoder_length_prefixed(&native(&[hex(VT_P2)]), false),
        Err(HevcError::ReferenceMismatch)
    );
    assert_eq!(
        encoder.normalize_encoder_length_prefixed(&native(&[hex(VT_P1)]), true),
        Err(HevcError::PictureMismatch)
    );
    encoder
        .normalize_encoder_length_prefixed(&native(&[hex(VT_P1)]), false)
        .unwrap();
}

#[test]
fn only_sei_is_removed_and_layer_and_parameter_restrictions_remain() {
    for extra in [vec![124, 1, 128], vec![78, 2, 128], hex(VT_PPS)] {
        let mut nals = startup();
        nals.insert(3, extra);
        assert!(
            guard()
                .normalize_encoder_length_prefixed(&native(&nals), true)
                .is_err()
        );
    }
    assert!(
        guard()
            .normalize_encoder_length_prefixed(&native(&[vec![78, 1, 128]]), true)
            .is_err()
    );
}

#[test]
fn original_input_and_nal_count_are_bounded_before_sei_removal() {
    let initial = native(&startup());
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_encoded_access_unit_bytes: Some(u32::try_from(initial.len()).unwrap()),
        ..LimitOverrides::default()
    })
    .unwrap();
    let mut small = HevcGuard::new(vt_config(), limits, 4).unwrap();
    let mut extra = startup();
    extra.push(vec![78, 1, 128]);
    assert_eq!(
        small.normalize_encoder_length_prefixed(&native(&extra), true),
        Err(HevcError::Limit)
    );
    small
        .normalize_encoder_length_prefixed(&initial, true)
        .unwrap();

    let mut maximum = startup();
    maximum.extend(std::iter::repeat_n(vec![78, 1, 128], 252));
    assert_eq!(
        guard()
            .normalize_encoder_length_prefixed(&native(&maximum), true)
            .unwrap(),
        initial
    );
    maximum.push(vec![78, 1, 128]);
    assert_eq!(
        guard().normalize_encoder_length_prefixed(&native(&maximum), true),
        Err(HevcError::Limit)
    );
}
