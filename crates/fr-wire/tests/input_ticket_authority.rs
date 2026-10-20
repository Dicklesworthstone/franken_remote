//! Exact optional-profile bytes and refusal boundaries; no transport or native
//! authority grant is established by these pure wire tests.
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
        INPUT_TICKET_BYTES, Ticket,
    },
    negotiation::{self, Capability, Offer, Role, Selection},
    presented,
};

const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;

fn ticket() -> Ticket {
    Ticket {
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
    }
}

fn authority() -> AuthorityTicket {
    AuthorityTicket {
        ticket: ticket(),
        authority_until_us: 2_000_006,
    }
}

fn legacy_golden() -> [u8; INPUT_TICKET_BYTES] {
    let hex = include_str!("fixtures/input_ticket/input_ticket.hex").trim();
    assert_eq!(hex.len(), 2 * INPUT_TICKET_BYTES);
    let mut bytes = [0; INPUT_TICKET_BYTES];
    for (out, pair) in bytes.iter_mut().zip(hex.as_bytes().as_chunks::<2>().0) {
        *out = u8::from_str_radix(core::str::from_utf8(pair).unwrap(), 16).unwrap();
    }
    bytes
}

fn encode(ticket: AuthorityTicket) -> [u8; AUTHORITY_TICKET_BYTES] {
    let mut out = [0; AUTHORITY_TICKET_BYTES];
    assert_eq!(
        input_ticket::encode_authority(
            ticket,
            &mut out,
            &LIMITS,
            7,
            Direction::HostToViewer,
            Delivery::Reliable,
        ),
        Ok(AUTHORITY_TICKET_BYTES)
    );
    out
}

fn decode(bytes: &[u8]) -> Result<AuthorityTicket, WireError> {
    input_ticket::decode_authority(
        bytes,
        &LIMITS,
        7,
        Direction::HostToViewer,
        Delivery::Reliable,
    )
}

fn limited(bytes: u32) -> ProtocolLimits {
    ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(bytes),
        ..LimitOverrides::default()
    })
    .unwrap()
}

fn selection() -> Selection {
    let mut capabilities: Vec<_> = [
        (AUTHORITY_CAPABILITY, AUTHORITY_VERSION),
        (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
        (control::GRANT_CAPABILITY, 1),
        (clock::CAPABILITY, clock::VERSION),
        (presented::CAPABILITY, presented::VERSION),
    ]
    .into_iter()
    .map(|(name, version)| Capability {
        name: name.to_owned(),
        version,
        required: name != AUTHORITY_CAPABILITY,
    })
    .collect();
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    Selection {
        version: 0,
        profile: negotiation::NATIVE_PROFILE,
        profile_version: negotiation::PROFILE_VERSION,
        role: Role::RequestControl,
        limits: LIMITS,
        capabilities,
    }
}

#[test]
fn existing_golden_is_unchanged_and_authority_appends_one_exact_field() {
    assert_eq!(INPUT_TICKET_BYTES, 128);
    assert_eq!(AUTHORITY_TICKET_BYTES, 136);
    let legacy = legacy_golden();
    let mut out = [0xa5; INPUT_TICKET_BYTES + 8];
    assert_eq!(
        input_ticket::encode(
            ticket(),
            &mut out,
            &LIMITS,
            7,
            Direction::HostToViewer,
            Delivery::Reliable,
        ),
        Ok(INPUT_TICKET_BYTES)
    );
    assert_eq!(&out[..INPUT_TICKET_BYTES], legacy);
    assert_eq!(&out[INPUT_TICKET_BYTES..], &[0xa5; 8]);
    assert_eq!(
        input_ticket::decode(
            &legacy,
            &LIMITS,
            7,
            Direction::HostToViewer,
            Delivery::Reliable
        ),
        Ok(ticket())
    );

    // Independent existing fixture plus the specified changed payload length
    // and one appended host-monotonic deadline; all legacy body bytes survive.
    let mut expected = legacy.to_vec();
    expected[12..16].copy_from_slice(&112_u32.to_be_bytes());
    expected.extend_from_slice(&2_000_006_u64.to_be_bytes());
    assert_eq!(encode(authority()).as_slice(), expected);
    assert_eq!(decode(&expected), Ok(authority()));
    let mut out = [0xa5; AUTHORITY_TICKET_BYTES + 8];
    assert_eq!(
        input_ticket::encode_authority(
            authority(),
            &mut out,
            &LIMITS,
            7,
            Direction::HostToViewer,
            Delivery::Reliable,
        ),
        Ok(AUTHORITY_TICKET_BYTES)
    );
    assert_eq!(&out[..AUTHORITY_TICKET_BYTES], expected);
    assert_eq!(&out[AUTHORITY_TICKET_BYTES..], &[0xa5; 8]);
}

#[test]
fn both_profiles_refuse_the_other_layout_instead_of_guessing_from_traffic() {
    assert_eq!(decode(&legacy_golden()), Err(WireError::Truncated));
    assert_eq!(
        input_ticket::decode(
            &encode(authority()),
            &LIMITS,
            7,
            Direction::HostToViewer,
            Delivery::Reliable,
        ),
        Err(WireError::TrailingBytes)
    );
}

#[test]
fn every_truncation_trailing_byte_wrong_role_channel_and_header_refuse() {
    let bytes = encode(authority());
    for end in 0..bytes.len() {
        assert!(decode(&bytes[..end]).is_err(), "truncation at {end}");
        assert_eq!(
            input_ticket::encode_authority(
                authority(),
                &mut vec![0; end],
                &LIMITS,
                7,
                Direction::HostToViewer,
                Delivery::Reliable,
            ),
            Err(WireError::BufferTooSmall)
        );
    }
    let mut extra = bytes.to_vec();
    extra.push(0);
    assert!(decode(&extra).is_err());
    extra[12..16].copy_from_slice(&113_u32.to_be_bytes());
    assert!(decode(&extra).is_err());
    for direction in [Direction::HostToViewer, Direction::ViewerToHost] {
        for delivery in [Delivery::Reliable, Delivery::Datagram] {
            if direction == Direction::HostToViewer && delivery == Delivery::Reliable {
                continue;
            }
            let expected = if direction == Direction::ViewerToHost {
                WireError::WrongRole
            } else {
                WireError::WrongChannel
            };
            assert_eq!(
                input_ticket::decode_authority(&bytes, &LIMITS, 7, direction, delivery),
                Err(expected)
            );
            assert_eq!(
                input_ticket::encode_authority(
                    authority(),
                    &mut [0; AUTHORITY_TICKET_BYTES],
                    &LIMITS,
                    7,
                    direction,
                    delivery,
                ),
                Err(expected)
            );
        }
    }
    for (offset, value) in [(0, 0), (5, 1), (7, 0x48), (8, 1), (19, 8), (23, 1)] {
        let mut bad = bytes;
        bad[offset] = value;
        assert!(decode(&bad).is_err(), "header byte {offset}");
    }
}

#[test]
fn identities_and_record_limits_apply_to_the_entire_extended_record() {
    let bytes = encode(authority());
    for start in [24, 40, 56] {
        let mut bad = bytes;
        bad[start..start + 16].fill(0);
        assert_eq!(decode(&bad), Err(WireError::InvalidBinding));
    }
    for channel in [0, 8] {
        assert!(
            input_ticket::decode_authority(
                &bytes,
                &LIMITS,
                channel,
                Direction::HostToViewer,
                Delivery::Reliable,
            )
            .is_err()
        );
    }
    assert_eq!(
        input_ticket::encode_authority(
            authority(),
            &mut [0; AUTHORITY_TICKET_BYTES],
            &LIMITS,
            0,
            Direction::HostToViewer,
            Delivery::Reliable,
        ),
        Err(WireError::InvalidBinding)
    );
    for maximum in [127, 128, 135, 136] {
        let limits = limited(maximum);
        let encoded = input_ticket::encode_authority(
            authority(),
            &mut [0; AUTHORITY_TICKET_BYTES],
            &limits,
            7,
            Direction::HostToViewer,
            Delivery::Reliable,
        );
        let decoded = input_ticket::decode_authority(
            &bytes,
            &limits,
            7,
            Direction::HostToViewer,
            Delivery::Reliable,
        );
        if maximum == 136 {
            assert_eq!(encoded, Ok(AUTHORITY_TICKET_BYTES));
            assert_eq!(decoded, Ok(authority()));
        } else {
            assert_eq!(encoded, Err(WireError::ResourceLimit));
            assert_eq!(decoded, Err(WireError::ResourceLimit));
        }
    }
}

#[test]
fn actual_authority_bounds_may_shorten_but_never_retime_the_enclosed_ticket() {
    let original = authority();
    for until in [1_000_006, 1_200_006, 2_000_006, 3_000_006] {
        let snapshot = AuthorityTicket {
            authority_until_us: until,
            ..original
        };
        let decoded = decode(&encode(snapshot)).unwrap();
        assert_eq!(decoded.authority_until_us, until);
        assert_eq!(decoded.ticket, original.ticket);
    }
    for until in [0, 5, 6, 1_000_005, 3_000_007, u64::MAX] {
        let bad = AuthorityTicket {
            authority_until_us: until,
            ..original
        };
        assert_eq!(
            input_ticket::encode_authority(
                bad,
                &mut [0; AUTHORITY_TICKET_BYTES],
                &LIMITS,
                7,
                Direction::HostToViewer,
                Delivery::Reliable,
            ),
            Err(WireError::InvalidValue)
        );
        let mut bytes = encode(original);
        bytes[128..136].copy_from_slice(&until.to_be_bytes());
        assert_eq!(decode(&bytes), Err(WireError::InvalidValue));
    }
    for expiry in [0_u64, 5, 6, 1_500_007, u64::MAX] {
        let mut bytes = encode(original);
        bytes[120..128].copy_from_slice(&expiry.to_be_bytes());
        assert_eq!(decode(&bytes), Err(WireError::InvalidValue));
    }
}

#[test]
fn arithmetic_boundaries_and_redaction_preserve_the_original_credential_contract() {
    for (issued, expires, until) in [
        (0, 1, 1),
        (u64::MAX - 10, u64::MAX - 1, u64::MAX),
        (
            u64::MAX - control::MAX_LEASE_LIFETIME_US,
            u64::MAX - input_ticket::MAX_TICKET_LIFETIME_US,
            u64::MAX,
        ),
    ] {
        let mut value = authority();
        value.ticket.issued_at_us = issued;
        value.ticket.expires_at_us = expires;
        value.authority_until_us = until;
        value.ticket.sequence = 0;
        value.ticket.credentials.view = InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        };
        assert_eq!(decode(&encode(value)), Ok(value));
        assert_eq!(format!("{value:?}"), "InputTicketAuthority([redacted])");
    }
    let mut bytes = encode(authority());
    bytes[112..120].copy_from_slice(&(u64::MAX - 10).to_be_bytes());
    bytes[120..128].copy_from_slice(&(u64::MAX - 1).to_be_bytes());
    for until in [0_u64, 5, u64::MAX - 11] {
        bytes[128..136].copy_from_slice(&until.to_be_bytes());
        assert_eq!(decode(&bytes), Err(WireError::InvalidValue));
    }
}

#[test]
fn positive_bilateral_optional_selection_preserves_legacy_peers() {
    let selected = selection();
    assert_eq!(input_ticket::authority_selected(&selected), Ok(true));
    let host = Offer {
        versions: vec![selected.version],
        profile: selected.profile,
        profile_version: selected.profile_version,
        role: selected.role,
        limits: selected.limits,
        capabilities: selected.capabilities,
    };
    let mut old_peer = host.clone();
    old_peer
        .capabilities
        .retain(|cap| cap.name != AUTHORITY_CAPABILITY);
    for intersection in [host.intersect(&old_peer), old_peer.intersect(&host)] {
        assert_eq!(
            input_ticket::authority_selected(&intersection.unwrap().select().unwrap()),
            Ok(false)
        );
    }
    let mut unsupported = host.clone();
    unsupported
        .capabilities
        .iter_mut()
        .find(|cap| cap.name == AUTHORITY_CAPABILITY)
        .unwrap()
        .version = 2;
    // Optional unsupported offers are not selected; an explicitly selected
    // unsupported version is a refusal, never an automatic legacy fallback.
    assert_eq!(
        input_ticket::authority_selected(&host.intersect(&unsupported).unwrap().select().unwrap()),
        Ok(false)
    );
    assert_eq!(
        input_ticket::authority_selected(&unsupported.select().unwrap()),
        Err(WireError::UnsupportedVersion)
    );
    let mut legacy = old_peer.select().unwrap();
    legacy.limits = limited(128);
    legacy.role = Role::Observe;
    legacy.capabilities.clear();
    assert_eq!(input_ticket::authority_selected(&legacy), Ok(false));
}

#[test]
fn explicit_selection_requires_current_role_dependencies_limits_and_valid_profile() {
    for dependency in [
        attachment::INPUT_CAPABILITY,
        control::GRANT_CAPABILITY,
        clock::CAPABILITY,
        presented::CAPABILITY,
    ] {
        let mut missing = selection();
        missing.capabilities.retain(|cap| cap.name != dependency);
        assert_eq!(
            input_ticket::authority_selected(&missing),
            Err(WireError::InvalidDependency)
        );
        let mut unsupported = selection();
        unsupported
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == dependency)
            .unwrap()
            .version += 1;
        assert_eq!(
            input_ticket::authority_selected(&unsupported),
            Err(WireError::InvalidDependency)
        );
    }
    let mut selected = selection();
    selected.role = Role::Observe;
    assert_eq!(
        input_ticket::authority_selected(&selected),
        Err(WireError::WrongRole)
    );
    selected = selection();
    selected.limits = limited(135);
    assert_eq!(
        input_ticket::authority_selected(&selected),
        Err(WireError::InvalidLimits)
    );
    selected.limits = limited(136);
    assert_eq!(input_ticket::authority_selected(&selected), Ok(true));
    for version in [2, u16::MAX] {
        let mut selected = selection();
        selected
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == AUTHORITY_CAPABILITY)
            .unwrap()
            .version = version;
        assert_eq!(
            input_ticket::authority_selected(&selected),
            Err(WireError::UnsupportedVersion)
        );
    }
    let mut selected = selection();
    selected.profile_version += 1;
    assert_eq!(
        input_ticket::authority_selected(&selected),
        Err(WireError::UnsupportedVersion)
    );
    let mut selected = selection();
    selected.capabilities.push(selected.capabilities[0].clone());
    assert_eq!(
        input_ticket::authority_selected(&selected),
        Err(WireError::InvalidValue)
    );
}
