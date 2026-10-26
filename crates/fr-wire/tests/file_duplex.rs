//! Duplex is a positive profile, never authority inferred from a file record.
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_wire::{
    HEADER_BYTES, WireError,
    attachment::{self, Descriptor, MediaRole},
    decoder::Binding,
    files,
    input::{InputDelivery, InputDirection},
    negotiation::{Capability, ControlBinding, Offer, Role, Selection},
};

const REQUIRED: [(&str, u16); 7] = [
    (attachment::CAPABILITY, attachment::VERSION),
    (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
    (attachment::FILES_CAPABILITY, attachment::FILES_VERSION),
    (files::CAPABILITY, files::VERSION),
    (
        files::CHANNEL_SCOPE_CAPABILITY,
        files::CHANNEL_SCOPE_VERSION,
    ),
    (files::DOWNLOAD_CAPABILITY, files::DOWNLOAD_VERSION),
    (files::DUPLEX_CAPABILITY, files::DUPLEX_VERSION),
];

fn offer() -> Offer {
    let mut capabilities: Vec<_> = REQUIRED
        .iter()
        .map(|&(name, version)| Capability {
            name: name.into(),
            version,
            required: false,
        })
        .collect();
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::RequestControl,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities,
    }
}
fn selection() -> Selection {
    offer().select().unwrap()
}

#[test]
fn duplex_requires_bilateral_exact_profiles_and_the_original_control_role() {
    let complete = selection();
    assert!(files::duplex_selected(&complete));
    for (name, _) in REQUIRED {
        for unknown_version in [false, true] {
            let mut peer = offer();
            if unknown_version {
                peer.capabilities
                    .iter_mut()
                    .find(|cap| cap.name == name)
                    .unwrap()
                    .version += 1;
            } else {
                peer.capabilities.retain(|cap| cap.name != name);
            }
            let selected = offer().intersect(&peer).unwrap().select().unwrap();
            assert!(!files::duplex_selected(&selected), "missing exact {name}");
        }
    }
    let mut observer = complete.clone();
    observer.role = Role::Observe;
    assert!(!files::duplex_selected(&observer));
    let mut duplicate = complete.clone();
    duplicate
        .capabilities
        .push(duplicate.capabilities[0].clone());
    assert!(duplicate.validate().is_err());
    assert!(!files::duplex_selected(&duplicate));
    let mut unordered = complete;
    unordered.capabilities.reverse();
    assert!(unordered.validate().is_err());
    assert!(!files::duplex_selected(&unordered));
}

fn parent() -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    }
}

#[test]
fn download_role_has_distinct_wire_identity_without_reassigning_audio_up() {
    let descriptor = Descriptor {
        binding: Binding {
            parent: ControlBinding { id: 9, ..parent() },
            display: 14,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
        },
        role: MediaRole::FilesDownload,
        host_stream: 7,
        viewer_stream: 6,
    };
    let mut bytes = [0; attachment::BINDING_RECORD_BYTES];
    let size = attachment::encode(
        attachment::Message::Binding(descriptor),
        parent(),
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(size, bytes.len());
    // Independently construct the complete canonical FRD0 Binding, including
    // its original control scope. No encoder output supplies expected bytes.
    let mut expected = b"FRD0\0\0".to_vec();
    expected.extend(0x001b_u16.to_be_bytes());
    expected.extend([0; 4]);
    expected.extend(118_u32.to_be_bytes());
    expected.extend(7_u32.to_be_bytes());
    expected.extend([0; 4]);
    expected.extend(9_u32.to_be_bytes());
    for identity in [11_u128, 12, 13, 14] {
        expected.extend(identity.to_be_bytes());
    }
    expected.extend([0; 32]);
    expected.extend([9, 1]);
    expected.extend(7_u64.to_be_bytes());
    expected.extend(6_u64.to_be_bytes());
    assert_eq!(expected.len(), 142);
    assert_eq!(bytes.as_slice(), expected);
    // Header + binding ID + boot/session/session/display + four generations.
    let role_offset = HEADER_BYTES + 4 + 4 * 16 + 4 * 8;
    assert_eq!(&bytes[role_offset..role_offset + 2], &[9, 1]);
    let decode = |bytes: &[u8]| {
        attachment::decode(
            bytes,
            parent(),
            parent().id,
            &ProtocolLimits::ABSOLUTE,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
    };
    assert_eq!(decode(&bytes), Ok(attachment::Message::Binding(descriptor)));
    for end in 0..expected.len() {
        assert!(decode(&expected[..end]).is_err(), "truncated prefix {end}");
    }
    expected.push(0);
    assert!(
        decode(&expected).is_err(),
        "trailing bytes are not another frame"
    );
    assert_eq!(MediaRole::Files as u8, 6);
    assert_eq!(MediaRole::Files.primary_direction(), 2);
    assert_eq!(MediaRole::FilesDownload.primary_direction(), 1);
    bytes[role_offset] = attachment::AUDIO_UP_RESERVED_ROLE;
    assert_eq!(decode(&bytes), Err(WireError::UnsupportedKind));
    bytes[role_offset] = 9;
    bytes[role_offset + 1] = 2;
    assert!(
        decode(&bytes).is_err(),
        "role cannot claim its sibling's direction"
    );
}

#[test]
fn file_envelopes_cannot_cross_direction_or_replace_original_scope() {
    let limits = files::Limits::new(&ProtocolLimits::ABSOLUTE, 4096).unwrap();
    let upload = files::Context {
        session: RemoteSessionId::from_raw(13),
        lease: InputLeaseId::from_raw(14),
        handle: 15,
        channel: 9,
        sender: files::Role::Controller,
        direction: files::Direction::ToHost,
        lane: files::Lane::Files,
    };
    let download = files::Context {
        handle: 16,
        channel: 10,
        sender: files::Role::Host,
        direction: files::Direction::ToController,
        ..upload
    };
    for (original, sibling) in [(upload, download), (download, upload)] {
        let message = files::Message {
            id: 1,
            body: files::Body::Offer {
                profile: files::ATP_PORTABLE_FULL,
                atp: b"envelope-only",
            },
        };
        let mut bytes = [0; 256];
        let size = files::encode(message, original, limits, &mut bytes).unwrap();
        let bytes = &bytes[..size];
        assert_eq!(files::decode(bytes, original, limits), Ok(message));
        for changed in [
            sibling,
            files::Context {
                session: RemoteSessionId::from_raw(99),
                ..original
            },
            files::Context {
                lease: InputLeaseId::from_raw(99),
                ..original
            },
            files::Context {
                handle: 99,
                ..original
            },
            files::Context {
                channel: 99,
                ..original
            },
            files::Context {
                sender: files::Role::Observer,
                ..original
            },
            files::Context {
                direction: sibling.direction,
                ..original
            },
        ] {
            assert!(files::decode(bytes, changed, limits).is_err());
        }
    }
}
