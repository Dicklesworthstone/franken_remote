//! Three explicit ticket layouts; an acknowledgement is never inferred from
//! length or proof of current local visibility. Runtime causality is separate.
use fr_core::{
    ids::*,
    input::{InputCredentials, InputView},
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{
    WireError, attachment, clock, control,
    input::{InputDelivery as Delivery, InputDirection as Direction},
    input_ticket::{
        self, AUTHORITY_CAPABILITY, AUTHORITY_TICKET_BYTES, AUTHORITY_VERSION, AuthorityTicket,
        INPUT_TICKET_BYTES, Profile, READINESS_CAPABILITY, READINESS_TICKET_BYTES,
        READINESS_VERSION, ReadinessTicket, Ticket,
    },
    negotiation::{self, Capability, Offer, Role, Selection},
    presented,
};

const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;
const PROFILES: [Profile; 3] = [Profile::Legacy, Profile::Authority, Profile::Readiness];

fn proof() -> ReadinessTicket {
    ReadinessTicket {
        authority: AuthorityTicket {
            ticket: Ticket {
                credentials: InputCredentials {
                    session: RemoteSessionId::from_raw(u128::from_be_bytes([0x11; 16])),
                    lease: InputLeaseId::from_raw(u128::from_be_bytes([0x22; 16])),
                    ticket: InputTicketId::from_raw(u128::from_be_bytes([0x33; 16])),
                    view: InputView {
                        geometry: DisplayGeometryGeneration::from_raw(1),
                        viewport: ViewportMappingGeneration::from_raw(2),
                        configuration: CodecConfigurationGeneration::from_raw(3),
                        recovery: RecoveryGeneration::from_raw(4),
                    },
                },
                sequence: 5,
                issued_at_us: 6,
                expires_at_us: 1_000_006,
            },
            authority_until_us: 2_000_006,
        },
        presented_sequence: 0x0102_0304_0506_0708,
    }
}

fn golden(profile: Profile) -> Vec<u8> {
    let hex = include_str!("fixtures/input_ticket/input_ticket.hex").trim();
    let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
    assert_eq!(remainder, []);
    let mut bytes: Vec<_> = pairs
        .iter()
        .map(|pair| u8::from_str_radix(core::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    assert_eq!(bytes.len(), INPUT_TICKET_BYTES);
    // Keep the independently written legacy fixture; append only the fields
    // specified by each profile, with literal fixed body lengths.
    if profile != Profile::Legacy {
        bytes[12..16].copy_from_slice(&112_u32.to_be_bytes());
        bytes.extend_from_slice(&2_000_006_u64.to_be_bytes());
    }
    if profile == Profile::Readiness {
        bytes[12..16].copy_from_slice(&120_u32.to_be_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    }
    bytes
}

fn encode(profile: Profile, value: ReadinessTicket, out: &mut [u8]) -> Result<usize, WireError> {
    match profile {
        Profile::Legacy => input_ticket::encode(
            value.authority.ticket,
            out,
            &LIMITS,
            7,
            Direction::HostToViewer,
            Delivery::Reliable,
        ),
        Profile::Authority => input_ticket::encode_authority(
            value.authority,
            out,
            &LIMITS,
            7,
            Direction::HostToViewer,
            Delivery::Reliable,
        ),
        Profile::Readiness => input_ticket::encode_readiness(
            value,
            out,
            &LIMITS,
            7,
            Direction::HostToViewer,
            Delivery::Reliable,
        ),
    }
}

fn decode(bytes: &[u8]) -> Result<ReadinessTicket, WireError> {
    input_ticket::decode_readiness(
        bytes,
        &LIMITS,
        7,
        Direction::HostToViewer,
        Delivery::Reliable,
    )
}

fn decode_profile(profile: Profile, bytes: &[u8]) -> Result<Ticket, WireError> {
    match profile {
        Profile::Legacy => input_ticket::decode(
            bytes,
            &LIMITS,
            7,
            Direction::HostToViewer,
            Delivery::Reliable,
        ),
        Profile::Authority => input_ticket::decode_authority(
            bytes,
            &LIMITS,
            7,
            Direction::HostToViewer,
            Delivery::Reliable,
        )
        .map(|value| value.ticket),
        Profile::Readiness => decode(bytes).map(|value| value.authority.ticket),
    }
}

fn limited(bytes: u32) -> ProtocolLimits {
    ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(bytes),
        ..LimitOverrides::default()
    })
    .unwrap()
}

fn offer(profile: Profile) -> Offer {
    let mut capabilities: Vec<_> = [
        (AUTHORITY_CAPABILITY, AUTHORITY_VERSION),
        (READINESS_CAPABILITY, READINESS_VERSION),
        (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
        (control::GRANT_CAPABILITY, 1),
        (clock::CAPABILITY, clock::VERSION),
        (presented::CAPABILITY, presented::VERSION),
    ]
    .into_iter()
    .filter(|(name, _)| {
        (*name != AUTHORITY_CAPABILITY || profile != Profile::Legacy)
            && (*name != READINESS_CAPABILITY || profile == Profile::Readiness)
    })
    .map(|(name, version)| Capability {
        name: name.to_owned(),
        version,
        required: !matches!(name, AUTHORITY_CAPABILITY | READINESS_CAPABILITY),
    })
    .collect();
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    Offer {
        versions: vec![0],
        profile: negotiation::NATIVE_PROFILE,
        profile_version: negotiation::PROFILE_VERSION,
        role: Role::RequestControl,
        limits: LIMITS,
        capabilities,
    }
}

fn selection() -> Selection {
    offer(Profile::Readiness).select().unwrap()
}

#[test]
fn all_three_layouts_match_the_original_golden_and_their_exact_appended_fields() {
    assert_eq!(
        (
            INPUT_TICKET_BYTES,
            AUTHORITY_TICKET_BYTES,
            READINESS_TICKET_BYTES
        ),
        (128, 136, 144)
    );
    for profile in PROFILES {
        let expected = golden(profile);
        let mut bytes = [0xa5; READINESS_TICKET_BYTES + 8];
        assert_eq!(encode(profile, proof(), &mut bytes), Ok(expected.len()));
        assert_eq!(&bytes[..expected.len()], expected);
        assert!(bytes[expected.len()..].iter().all(|byte| *byte == 0xa5));
        assert_eq!(
            decode_profile(profile, &expected),
            Ok(proof().authority.ticket)
        );
    }
    assert_eq!(decode(&golden(Profile::Readiness)), Ok(proof()));
    assert_eq!(format!("{:?}", proof()), "InputTicketReadiness([redacted])");
}

#[test]
fn every_cross_profile_combination_refuses_without_a_fallback_decoder() {
    for encoded in PROFILES {
        for decoded in PROFILES {
            if encoded == decoded {
                continue;
            }
            let bytes = golden(encoded);
            let error = if bytes.len() < golden(decoded).len() {
                WireError::Truncated
            } else {
                WireError::TrailingBytes
            };
            assert_eq!(
                decode_profile(decoded, &bytes),
                Err(error),
                "{encoded:?} -> {decoded:?}"
            );
        }
    }
}

#[test]
fn readiness_has_one_fixed_size_and_preserves_route_header_and_limit_checks() {
    let bytes = golden(Profile::Readiness);
    for end in 0..READINESS_TICKET_BYTES {
        assert_eq!(decode(&bytes[..end]), Err(WireError::Truncated));
        assert_eq!(
            encode(Profile::Readiness, proof(), &mut vec![0; end]),
            Err(WireError::BufferTooSmall)
        );
    }
    for corrected_length in [false, true] {
        let mut extra = bytes.clone();
        extra.push(0);
        if corrected_length {
            extra[12..16].copy_from_slice(&121_u32.to_be_bytes());
        }
        assert_eq!(decode(&extra), Err(WireError::TrailingBytes));
    }
    for (direction, delivery, error) in [
        (
            Direction::ViewerToHost,
            Delivery::Reliable,
            WireError::WrongRole,
        ),
        (
            Direction::ViewerToHost,
            Delivery::Datagram,
            WireError::WrongRole,
        ),
        (
            Direction::HostToViewer,
            Delivery::Datagram,
            WireError::WrongChannel,
        ),
    ] {
        assert_eq!(
            input_ticket::decode_readiness(&bytes, &LIMITS, 7, direction, delivery),
            Err(error)
        );
        assert_eq!(
            input_ticket::encode_readiness(
                proof(),
                &mut [0; READINESS_TICKET_BYTES],
                &LIMITS,
                7,
                direction,
                delivery
            ),
            Err(error)
        );
    }
    for (offset, value) in [(0, 0), (5, 1), (7, 0x48), (8, 1), (19, 8), (23, 1)] {
        let mut bad = bytes.clone();
        bad[offset] = value;
        assert!(decode(&bad).is_err(), "header byte {offset}");
    }
    for channel in [0, 8] {
        assert!(
            input_ticket::decode_readiness(
                &bytes,
                &LIMITS,
                channel,
                Direction::HostToViewer,
                Delivery::Reliable
            )
            .is_err()
        );
    }
    for maximum in [128, 136, 143, 144] {
        let limits = limited(maximum);
        let encoded = input_ticket::encode_readiness(
            proof(),
            &mut [0; READINESS_TICKET_BYTES],
            &limits,
            7,
            Direction::HostToViewer,
            Delivery::Reliable,
        );
        let decoded = input_ticket::decode_readiness(
            &bytes,
            &limits,
            7,
            Direction::HostToViewer,
            Delivery::Reliable,
        );
        if maximum == 144 {
            assert_eq!(encoded, Ok(READINESS_TICKET_BYTES));
            assert_eq!(decoded, Ok(proof()));
        } else {
            assert_eq!(encoded, Err(WireError::ResourceLimit));
            assert_eq!(decoded, Err(WireError::ResourceLimit));
        }
    }
}

#[test]
fn zero_proof_refuses_and_a_proof_cannot_relax_the_original_ticket_or_authority() {
    for sequence in [1, u64::MAX] {
        let value = ReadinessTicket {
            presented_sequence: sequence,
            ..proof()
        };
        let mut bytes = [0; READINESS_TICKET_BYTES];
        encode(Profile::Readiness, value, &mut bytes).unwrap();
        assert_eq!(decode(&bytes), Ok(value));
    }
    let zero = ReadinessTicket {
        presented_sequence: 0,
        ..proof()
    };
    assert_eq!(
        encode(Profile::Readiness, zero, &mut [0; READINESS_TICKET_BYTES]),
        Err(WireError::InvalidValue)
    );
    let mut bytes = golden(Profile::Readiness);
    bytes[136..144].fill(0);
    assert_eq!(decode(&bytes), Err(WireError::InvalidValue));

    for authority_until in [0, 1_000_005, 3_000_007, u64::MAX] {
        let mut value = proof();
        value.authority.authority_until_us = authority_until;
        assert_eq!(
            encode(Profile::Readiness, value, &mut [0; READINESS_TICKET_BYTES]),
            Err(WireError::InvalidValue)
        );
        let mut bytes = golden(Profile::Readiness);
        bytes[128..136].copy_from_slice(&authority_until.to_be_bytes());
        assert_eq!(decode(&bytes), Err(WireError::InvalidValue));
    }
    let mut value = proof();
    value.authority.ticket.expires_at_us = 1_500_007;
    assert_eq!(
        encode(Profile::Readiness, value, &mut [0; READINESS_TICKET_BYTES]),
        Err(WireError::InvalidValue)
    );
    for start in [24, 40, 56] {
        let mut bytes = golden(Profile::Readiness);
        bytes[start..start + 16].fill(0);
        assert_eq!(decode(&bytes), Err(WireError::InvalidBinding));
    }
    let mut value = proof();
    value.authority.ticket.issued_at_us = u64::MAX - 2;
    value.authority.ticket.expires_at_us = u64::MAX - 1;
    value.authority.authority_until_us = u64::MAX;
    let mut bytes = [0; READINESS_TICKET_BYTES];
    encode(Profile::Readiness, value, &mut bytes).unwrap();
    assert_eq!(decode(&bytes), Ok(value));
    value.authority.authority_until_us = 1;
    assert_eq!(
        encode(Profile::Readiness, value, &mut bytes),
        Err(WireError::InvalidValue)
    );
}

#[test]
fn optional_bilateral_selection_keeps_both_older_profiles_and_exact_helpers() {
    let current = offer(Profile::Readiness);
    for profile in PROFILES {
        let peer = offer(profile);
        for intersection in [current.intersect(&peer), peer.intersect(&current)] {
            let selected = intersection.unwrap().select().unwrap();
            assert_eq!(input_ticket::selected_profile(&selected), Ok(profile));
            assert_eq!(
                input_ticket::authority_selected(&selected),
                Ok(profile != Profile::Legacy)
            );
            assert_eq!(
                input_ticket::readiness_selected(&selected),
                Ok(profile == Profile::Readiness)
            );
        }
    }
    let mut newer = current.clone();
    newer
        .capabilities
        .iter_mut()
        .find(|cap| cap.name == READINESS_CAPABILITY)
        .unwrap()
        .version = 2;
    for intersection in [current.intersect(&newer), newer.intersect(&current)] {
        assert_eq!(
            input_ticket::selected_profile(&intersection.unwrap().select().unwrap()),
            Ok(Profile::Authority)
        );
    }
    assert_eq!(
        input_ticket::selected_profile(&newer.select().unwrap()),
        Err(WireError::UnsupportedVersion)
    );
    let mut legacy = offer(Profile::Legacy).select().unwrap();
    legacy.role = Role::Observe;
    legacy.capabilities.clear();
    legacy.limits = limited(128);
    assert_eq!(input_ticket::selected_profile(&legacy), Ok(Profile::Legacy));
}

#[test]
fn explicit_readiness_requires_authority_dependencies_role_limits_and_valid_selection() {
    for dependency in [
        AUTHORITY_CAPABILITY,
        attachment::INPUT_CAPABILITY,
        control::GRANT_CAPABILITY,
        clock::CAPABILITY,
        presented::CAPABILITY,
    ] {
        let mut missing = selection();
        missing.capabilities.retain(|cap| cap.name != dependency);
        assert_eq!(
            input_ticket::selected_profile(&missing),
            Err(WireError::InvalidDependency)
        );
        // All convenience APIs must fail closed too, rather than returning a
        // boolean that silently chooses an older decoder.
        assert_eq!(
            input_ticket::authority_selected(&missing),
            Err(WireError::InvalidDependency)
        );
        assert_eq!(
            input_ticket::readiness_selected(&missing),
            Err(WireError::InvalidDependency)
        );
        let mut wrong = selection();
        wrong
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == dependency)
            .unwrap()
            .version += 1;
        let expected = if dependency == AUTHORITY_CAPABILITY {
            WireError::UnsupportedVersion
        } else {
            WireError::InvalidDependency
        };
        assert_eq!(input_ticket::selected_profile(&wrong), Err(expected));
    }
    let mut selected = selection();
    selected.role = Role::Observe;
    assert_eq!(
        input_ticket::selected_profile(&selected),
        Err(WireError::WrongRole)
    );
    for maximum in [136, 143, 144] {
        let mut selected = selection();
        selected.limits = limited(maximum);
        assert_eq!(
            input_ticket::selected_profile(&selected),
            if maximum == 144 {
                Ok(Profile::Readiness)
            } else {
                Err(WireError::InvalidLimits)
            }
        );
    }
    let mut selected = selection();
    selected
        .capabilities
        .iter_mut()
        .find(|cap| cap.name == READINESS_CAPABILITY)
        .unwrap()
        .version = 0;
    assert_eq!(
        input_ticket::selected_profile(&selected),
        Err(WireError::InvalidValue)
    );
    let mut selected = selection();
    selected.profile_version += 1;
    assert_eq!(
        input_ticket::selected_profile(&selected),
        Err(WireError::UnsupportedVersion)
    );
    let mut selected = selection();
    selected.capabilities.push(selected.capabilities[0].clone());
    assert_eq!(
        input_ticket::selected_profile(&selected),
        Err(WireError::InvalidValue)
    );
}
