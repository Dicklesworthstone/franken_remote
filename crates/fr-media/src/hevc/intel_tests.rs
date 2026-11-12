//! Real Intel Media Foundation HEVC transform output ("Intel® Hardware H265
//! Encoder MFT", Iris Plus, Windows 11) from a Desktop Duplication capture at
//! 2256x1504: CBR, low latency, no B pictures requested. Parameter sets are
//! whole. Slices keep only their header; the slice data is synthetic filler,
//! so no screen content.
//!
//! The transform emits low-delay B slices (both lists hold only past
//! pictures), two active references per list from an SPS set, inert
//! long-term and list-modification syntax, and no colour description, and
//! it refuses every setting that would change those.
use super::*;

const VPS: &str = "40010c01ffff0140000003009000000300000300963c0c0000030004000003007940";
const SPS: &str = concat!(
    "420101014000000300900000030000030096a0011a2005e168f9246d93ff6028010000030001",
    "000003001e61b5ef7e000f424001e84c80",
);
const PPS: &str = "4401c02564c0ed90";
const FILLER: &str = "a5a5a5a5";
/// IDR, then the first B: an inline set with the previous picture only.
const IDR: &str = "2601afb0";
const B1: &str = "0201e117feec";
/// Later Bs select the SPS set (previous two pictures, both active).
fn later_b(poc: u32) -> String {
    format!("0201e{poc:x}f87b{FILLER}")
}
fn slice(header: &str) -> String {
    format!("{header}{FILLER}")
}

fn config() -> CodecConfiguration {
    CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::INITIAL,
        CodedGeometry::from_visible(&ProtocolLimits::ABSOLUTE, 2256, 1504, 16).unwrap(),
        ColorInfo::sdr_bt709(),
        GopPolicy::baseline_for_frame_rate(30).unwrap(),
    )
    .unwrap()
}
fn described() -> Vec<u8> {
    describe_encoder_colour(&hex(SPS), ColorInfo::sdr_bt709()).unwrap()
}
fn startup_with(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    au(&[&hex(VPS), sps, pps, &hex(&slice(IDR))])
}
/// A guard that admitted the IDR and B1 (two pictures of history).
fn running() -> HevcGuard {
    let mut g = HevcGuard::new(config(), ProtocolLimits::ABSOLUTE, 4).unwrap();
    g.validate_annex_b(&startup_with(&described(), &hex(PPS)), true)
        .unwrap();
    g.validate_annex_b(&au(&[&hex(&slice(B1))]), false).unwrap();
    g
}

#[test]
fn real_intel_mft_stream_is_admitted_once_the_host_describes_its_colour() {
    let raw = startup_with(&hex(SPS), &hex(PPS));
    let mut g = HevcGuard::new(config(), ProtocolLimits::ABSOLUTE, 4).unwrap();
    assert_eq!(
        g.validate_annex_b(&raw, true),
        Err(HevcError::ColorMismatch)
    );

    let sps = described();
    let normalized = describe_encoder_colour_annex_b(&raw, ColorInfo::sdr_bt709()).unwrap();
    assert_eq!(normalized, startup_with(&sps, &hex(PPS)));
    // Already described: returned unchanged, never rewritten.
    assert_eq!(
        describe_encoder_colour(&sps, ColorInfo::sdr_bt709()).unwrap(),
        sps
    );

    let info = g.validate_annex_b(&normalized, true).unwrap();
    assert!(info.idr);
    assert_eq!(info.level_idc, 150);
    assert_eq!(
        g.validate_annex_b(&au(&[&hex(&slice(B1))]), false)
            .unwrap()
            .poc_lsb,
        1
    );
    for poc in 2..=7 {
        assert_eq!(
            g.validate_annex_b(&au(&[&hex(&later_b(poc))]), false)
                .unwrap()
                .poc_lsb,
            poc
        );
    }
    // A client admits the same stream from the record the host derives.
    let record = g.decoder_record().unwrap();
    let mut client =
        HevcGuard::from_decoder_record(config(), ProtocolLimits::ABSOLUTE, 4, record.bytes())
            .unwrap();
    assert!(client.validate_annex_b(&normalized, true).unwrap().idr);
    assert_eq!(
        client
            .validate_annex_b(&au(&[&hex(&slice(B1))]), false)
            .unwrap()
            .poc_lsb,
        1
    );
}

#[test]
fn colour_description_states_only_what_the_host_fed_the_encoder() {
    // The encoder's studio range disagrees with a full-range configuration:
    // refused, not rewritten.
    let full = ColorInfo {
        range: crate::config::ColorRange::Full,
        ..ColorInfo::sdr_bt709()
    };
    assert_eq!(
        describe_encoder_colour(&hex(SPS), full),
        Err(HevcError::ColorMismatch)
    );
    // An encoder's own (different) description is kept, so admission still
    // refuses it.
    let unspecified = hex(super::UNSPECIFIED_COLOR_SPS);
    assert_eq!(
        describe_encoder_colour(&unspecified, ColorInfo::sdr_bt709()).unwrap(),
        unspecified
    );
    // No video signal type at all (flag at SPS 228): the whole signal type is
    // written, and the result is admitted.
    let bare = planted(SPS, |b| {
        b[228] = false;
        b.drain(229..234);
    });
    let sps = describe_encoder_colour(&bare, ColorInfo::sdr_bt709()).unwrap();
    assert_ne!(sps, described());
    let mut g = HevcGuard::new(config(), ProtocolLimits::ABSOLUTE, 4).unwrap();
    assert!(
        g.validate_annex_b(&startup_with(&sps, &hex(PPS)), true)
            .unwrap()
            .idr
    );
    // Only an SPS is described.
    assert_eq!(
        describe_encoder_colour(&hex(PPS), ColorInfo::sdr_bt709()),
        Err(HevcError::UnsupportedSyntax)
    );
}

#[test]
fn low_delay_b_admission_keeps_reordering_long_term_and_list_edits_refused() {
    let sps = described();
    let start = |sps: &[u8], pps: &[u8]| {
        HevcGuard::new(config(), ProtocolLimits::ABSOLUTE, 4)
            .unwrap()
            .validate_annex_b(&startup_with(sps, pps), true)
            .map(|_| ())
    };
    assert_eq!(start(&sps, &hex(PPS)), Ok(()));
    // SPS (positions from the NAL header): a future picture in the set
    // (num_positive at 208); a long-term candidate (count at 214).
    let future = planted(&hex_of(&sps), |b| insert(b, 208, "01"));
    assert_eq!(start(&future, &hex(PPS)), Err(HevcError::ReferenceMismatch));
    let candidate = planted(&hex_of(&sps), |b| {
        b[214] = false;
        insert(b, 215, "10");
    });
    assert_eq!(start(&candidate, &hex(PPS)), Err(HevcError::Limit));
    // PPS: three default active references (L0 count at 25..28).
    let three = planted(PPS, |b| b[27] = true);
    assert_eq!(start(&sps, &three), Err(HevcError::Limit));

    let next = |nal: &[u8]| {
        running()
            .validate_annex_b(&au(&[nal]), false)
            .map(|info| info.poc_lsb)
    };
    let b2 = later_b(2);
    assert_eq!(next(&hex(&b2)), Ok(2));
    // B2 header (from the NAL header): long-term count 25, L0/L1
    // modification flags 30/31, collocated_ref_idx 34.
    let long_term = planted(&b2, |b| {
        b[25] = false;
        insert(b, 26, "10");
    });
    assert_eq!(next(&long_term), Err(HevcError::Limit));
    for at in [30, 31] {
        assert_eq!(
            next(&planted(&b2, |b| b[at] = true)),
            Err(HevcError::UnsupportedSyntax),
            "modification flag {at}"
        );
    }
    let override_three = planted(&b2, |b| {
        b[29] = true;
        insert(b, 30, "011 1");
    });
    assert_eq!(next(&override_three), Err(HevcError::Limit));
    // collocated_ref_idx 1 is inside the two active references; 2 is not.
    // Six zero bits keep the slice data byte-aligned after the longer code.
    let collocated = |code: &str| {
        planted(&b2, |b| {
            b[34] = false;
            insert(b, 35, code);
            insert(b, 42, "000000");
        })
    };
    assert_eq!(next(&collocated("10")), Ok(2));
    assert_eq!(next(&collocated("11")), Err(HevcError::Limit));
    // A non-IDR I slice (type at 18) is not a picture of this stream.
    assert_eq!(
        next(&planted(&b2, |b| {
            b[18] = false;
            insert(b, 19, "11");
        })),
        Err(HevcError::PictureMismatch)
    );
    // B1's inline set with a future picture (num_positive at 29).
    let mut g = HevcGuard::new(config(), ProtocolLimits::ABSOLUTE, 4).unwrap();
    g.validate_annex_b(&startup_with(&sps, &hex(PPS)), true)
        .unwrap();
    let ahead = planted(&slice(B1), |b| {
        b[29] = false;
        insert(b, 30, "10");
    });
    assert_eq!(
        g.validate_annex_b(&au(&[&ahead]), false),
        Err(HevcError::ReferenceMismatch)
    );
    // The SPS set needs two pictures of history: B2 straight after the IDR
    // is refused.
    assert_eq!(
        g.validate_annex_b(&au(&[&hex(&b2)]), false),
        Err(HevcError::ReferenceMismatch)
    );
}

fn hex_of(bytes: &[u8]) -> String {
    use core::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}
