use fr_core::{ids::*, limits::ProtocolLimits};
use fr_wire::{
    SourceObservation, WireError,
    decoder::Binding,
    input::{InputDelivery as D, InputDirection as I},
    negotiation::ControlBinding,
    presented::*,
};
fn binding() -> Binding {
    Binding {
        parent: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        },
        display: 4,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
fn report() -> Report {
    Report {
        sequence: 1,
        visible: Some(Sample {
            stamp: Stamp {
                frame: 9,
                captured_us: 10_000,
                observed_us: 20_000,
                source: SourceObservation::QualifiedUnchanged,
            },
            age_upper_us: 30_000,
        }),
    }
}
fn bytes(r: Report) -> Vec<u8> {
    let mut b = vec![0; BYTES];
    assert_eq!(
        encode(
            r,
            binding(),
            &ProtocolLimits::ABSOLUTE,
            &mut b,
            I::ViewerToHost,
            D::Reliable
        )
        .unwrap(),
        BYTES
    );
    b
}
fn read(b: &[u8]) -> Result<Report, WireError> {
    decode(
        b,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        I::ViewerToHost,
        D::Reliable,
    )
}
#[test]
fn exact_fixed_size_positive_and_unavailable_reports_roundtrip() {
    for r in [
        report(),
        Report {
            sequence: 2,
            visible: None,
        },
    ] {
        let b = bytes(r);
        assert_eq!(&b[6..8], &[0, 0x80]);
        assert_eq!(read(&b).unwrap(), r);
    }
}
#[test]
fn all_truncations_and_trailing_data_are_refused() {
    let b = bytes(report());
    for end in 0..b.len() {
        assert!(read(&b[..end]).is_err(), "prefix {end}");
    }
    let mut b = b;
    b.push(0);
    assert!(read(&b).is_err());
    let mut extended = bytes(report());
    extended[12..16].copy_from_slice(&147_u32.to_be_bytes());
    extended[20..24].copy_from_slice(&8_u32.to_be_bytes());
    extended.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
    assert_eq!(read(&extended), Err(WireError::TrailingBytes));
}
#[test]
fn wrong_direction_and_every_binding_generation_are_refused() {
    let b = bytes(report());
    assert_eq!(
        decode(
            &b,
            binding(),
            &ProtocolLimits::ABSOLUTE,
            I::HostToViewer,
            D::Reliable
        ),
        Err(WireError::WrongRole)
    );
    for n in 0..8 {
        let mut x = binding();
        match n {
            0 => x.parent.host_boot = HostBootId::from_raw(99),
            1 => x.parent.os_session = OsSessionId::from_raw(99),
            2 => x.parent.remote_session = RemoteSessionId::from_raw(99),
            3 => x.display = 99,
            4 => x.geometry = x.geometry.next().unwrap(),
            5 => x.configuration = x.configuration.next().unwrap(),
            6 => x.recovery = x.recovery.next().unwrap(),
            _ => x.viewport = x.viewport.next().unwrap(),
        }
        assert!(
            decode(
                &b,
                x,
                &ProtocolLimits::ABSOLUTE,
                I::ViewerToHost,
                D::Reliable
            )
            .is_err()
        );
    }
}
#[test]
fn unavailable_has_no_smuggled_source_and_unknown_stage_never_means_visible() {
    let b = bytes(report());
    let stage = fr_wire::HEADER_BYTES + fr_wire::decoder::BINDING_BYTES + 9;
    for tag in [0, 2, 3, 255] {
        let mut bad = b.clone();
        bad[stage] = tag;
        assert!(read(&bad).is_err());
    }
    let mut b = bytes(Report {
        sequence: 1,
        visible: None,
    });
    *b.last_mut().unwrap() = 1;
    assert!(read(&b).is_err());
}
#[test]
fn impossible_source_clock_uncertainty_and_zero_sequence_are_rejected() {
    let mut r = report();
    for invalid in 0..4 {
        let mut s = report().visible.unwrap();
        match invalid {
            0 => s.stamp.source = SourceObservation::Unknown,
            1 => s.stamp.source = SourceObservation::Captured,
            2 => s.stamp.observed_us = 1,
            _ => s.age_upper_us = MAX_SOURCE_AGE_US,
        }
        r.visible = Some(s);
        assert!(
            encode(
                r,
                binding(),
                &ProtocolLimits::ABSOLUTE,
                &mut [0; BYTES],
                I::ViewerToHost,
                D::Reliable
            )
            .is_err()
        );
        r = report();
    }
    r.sequence = 0;
    assert!(
        encode(
            r,
            binding(),
            &ProtocolLimits::ABSOLUTE,
            &mut [0; BYTES],
            I::ViewerToHost,
            D::Reliable
        )
        .is_err()
    );
}

#[test]
fn literal_network_order_fixture_and_datagram_refusal() {
    // Independent literal layout: 24-byte header, four u128 identities, four
    // u64 generations, then the 43-byte v1 presentation payload.
    let hex = concat!(
        "4652443000000080000000000000008b0000000700000000",
        "00000000000000000000000000000001",
        "00000000000000000000000000000002",
        "00000000000000000000000000000003",
        "00000000000000000000000000000004",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0100000000000000010100000000000000090000000000002710",
        "0000000000004e20020000000000007530",
    );
    let fixture: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|n| u8::from_str_radix(&hex[n..n + 2], 16).unwrap())
        .collect();
    assert_eq!(fixture.len(), BYTES);
    assert_eq!(fixture, bytes(report()));
    assert_eq!(read(&fixture).unwrap(), report());
    assert_eq!(
        decode(
            &fixture,
            binding(),
            &ProtocolLimits::ABSOLUTE,
            I::ViewerToHost,
            D::Datagram
        ),
        Err(WireError::WrongChannel)
    );
    let mut wrong = binding();
    wrong.parent.id += 1;
    assert!(
        decode(
            &fixture,
            wrong,
            &ProtocolLimits::ABSOLUTE,
            I::ViewerToHost,
            D::Reliable
        )
        .is_err()
    );
}
