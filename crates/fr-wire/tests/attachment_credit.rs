//! Independent wire fixtures and exact reservation intersection, not OS or QUIC
//! evidence. Header/body bytes are written from `NATIVE_ATTACHMENT_CREDIT.md`.
use fr_core::{
    ids::*,
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{
    WireError,
    attachment::{
        self, Descriptor, Grant, MediaRole, Ticket,
        credit::{self, Accepted},
    },
    decoder::Binding,
    input::{InputDelivery as T, InputDirection as D},
    negotiation::{Capability, ControlBinding, Offer, Role},
    stream::RecordStream,
};
const L: ProtocolLimits = ProtocolLimits::ABSOLUTE;
const GOLDEN: [u8; 36] = [
    0x46, 0x52, 0x44, 0x30, 0, 0, 0, 0x1c, 0, 0, 0, 0, 0, 0, 0, 12, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0,
    0, 8, 0, 0, 0, 0, 0, 0, 0x20, 0,
];
fn parent() -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    }
}
fn accepted() -> Accepted {
    Accepted {
        binding: 8,
        byte_allowance: 8192,
    }
}
fn grant(role: MediaRole, allowance: u64) -> Grant {
    Grant {
        descriptor: Descriptor {
            binding: Binding {
                parent: ControlBinding { id: 8, ..parent() },
                display: 14,
                geometry: DisplayGeometryGeneration::INITIAL,
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
                viewport: ViewportMappingGeneration::INITIAL,
            },
            role,
            host_stream: 7,
            viewer_stream: 6,
        },
        ticket: Ticket(0x1234_5678_90ab_cdef),
        deadline_us: 900,
        byte_allowance: allowance,
        picture_allowance: 0,
        credit_epoch: 8,
    }
}
fn decode(bytes: &[u8]) -> Result<Accepted, WireError> {
    credit::decode(bytes, parent(), &L, D::ViewerToHost, T::Reliable)
}
#[test]
fn independent_golden_and_every_stream_split_keep_the_exact_original_bytes() {
    assert_eq!(credit::ACCEPTED_BYTES, GOLDEN.len());
    let mut out = [0xa5; credit::ACCEPTED_BYTES + 8];
    let n = credit::encode(
        accepted(),
        parent(),
        &L,
        &mut out,
        D::ViewerToHost,
        T::Reliable,
    )
    .unwrap();
    assert_eq!(&out[..n], GOLDEN);
    assert_eq!(&out[n..], &[0xa5; 8]);
    assert_eq!(decode(&GOLDEN).unwrap(), accepted());
    for split in 1..GOLDEN.len() {
        let mut stream = RecordStream::new(credit::ACCEPTED_BYTES, 7, 100).unwrap();
        stream.push(&GOLDEN[..split], 10).unwrap();
        assert!(stream.frame(10).unwrap().is_none());
        stream.push(&GOLDEN[split..], 20).unwrap();
        assert_eq!(
            decode(stream.frame(20).unwrap().unwrap()).unwrap(),
            accepted()
        );
        assert_eq!(stream.next_deadline(), Some(110));
        stream.consume(20).unwrap();
        assert_eq!(stream.buffered_bytes(), 0);
    }
}
#[test]
fn legacy_and_extended_codecs_do_not_silently_accept_each_others_layout() {
    assert!(attachment::decode(&GOLDEN, parent(), 7, &L, D::ViewerToHost, T::Reliable).is_err());
    let mut out = [0; attachment::GRANT_RECORD_BYTES];
    let n = attachment::encode(
        attachment::Message::Accepted(8),
        parent(),
        &L,
        &mut out,
        D::ViewerToHost,
        T::Reliable,
    )
    .unwrap();
    assert!(decode(&out[..n]).is_err());
    assert_eq!(
        attachment::decode(&out[..n], parent(), 7, &L, D::ViewerToHost, T::Reliable).unwrap(),
        attachment::Message::Accepted(8)
    );
}
#[test]
fn truncation_trailing_data_wrong_role_lane_kind_binding_and_reserved_flags_refuse() {
    for n in 0..GOLDEN.len() {
        assert!(decode(&GOLDEN[..n]).is_err(), "truncation {n}");
        assert!(
            credit::encode(
                accepted(),
                parent(),
                &L,
                &mut vec![0; n],
                D::ViewerToHost,
                T::Reliable
            )
            .is_err()
        );
    }
    let mut extra = GOLDEN.to_vec();
    extra.push(0);
    assert!(decode(&extra).is_err());
    extra[15] = 13;
    assert!(decode(&extra).is_err());
    for sender in [D::HostToViewer, D::ViewerToHost] {
        for delivery in [T::Datagram, T::Reliable] {
            if sender == D::ViewerToHost && delivery == T::Reliable {
                continue;
            }
            assert!(credit::decode(&GOLDEN, parent(), &L, sender, delivery).is_err());
            assert!(
                credit::encode(accepted(), parent(), &L, &mut [0; 36], sender, delivery).is_err()
            );
        }
    }
    for (offset, value) in [
        (0, 0),
        (5, 1),
        (7, 0x18),
        (8, 1),
        (19, 9),
        (23, 1),
        (27, 0),
        (27, 7),
    ] {
        let mut bad = GOLDEN;
        bad[offset] = value;
        assert!(decode(&bad).is_err(), "header/body byte {offset}");
    }
}
#[test]
fn every_role_intersects_both_directions_without_mutating_the_reservation() {
    for role in [
        MediaRole::Configuration,
        MediaRole::Recovery,
        MediaRole::Video,
        MediaRole::Input,
        MediaRole::Clipboard,
        MediaRole::Files,
        MediaRole::FilesDownload,
        MediaRole::AudioDown,
    ] {
        for host in [186, 512, 1150, 8192, 65_536] {
            for client in [186, 512, 1150, 8192, 65_536] {
                let offered = grant(role, host);
                let result = Accepted {
                    binding: 8,
                    byte_allowance: client,
                }
                .negotiate(offered, parent(), &L)
                .unwrap();
                assert_eq!(
                    result,
                    Grant {
                        byte_allowance: host.min(client),
                        ..offered
                    }
                );
                assert_eq!(offered.byte_allowance, host);
                assert!(result.byte_allowance <= host && result.byte_allowance <= client);
            }
        }
    }
}
#[test]
fn invalid_limits_bindings_or_original_grants_cannot_be_laundered_by_a_small_peer_cap() {
    let original = grant(MediaRole::Clipboard, 65_536);
    for amount in [0, 1, 185, 65_537, u64::MAX] {
        let bad = Accepted {
            byte_allowance: amount,
            ..accepted()
        };
        assert!(bad.negotiate(original, parent(), &L).is_err());
        let mut bytes = GOLDEN;
        bytes[28..].copy_from_slice(&amount.to_be_bytes());
        assert!(decode(&bytes).is_err());
        assert!(
            credit::encode(
                bad,
                parent(),
                &L,
                &mut [0; 36],
                D::ViewerToHost,
                T::Reliable
            )
            .is_err()
        );
        assert!(
            accepted()
                .negotiate(
                    Grant {
                        byte_allowance: amount,
                        ..original
                    },
                    parent(),
                    &L
                )
                .is_err()
        );
    }
    for binding in [0, 7, 9, u32::MAX] {
        assert!(
            Accepted {
                binding,
                ..accepted()
            }
            .negotiate(original, parent(), &L)
            .is_err()
        );
    }
    for bad in [
        Grant {
            ticket: Ticket(0),
            ..original
        },
        Grant {
            deadline_us: 0,
            ..original
        },
        Grant {
            credit_epoch: 0,
            ..original
        },
        Grant {
            picture_allowance: 1,
            ..original
        },
    ] {
        assert!(accepted().negotiate(bad, parent(), &L).is_err());
    }
    let small = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(4096),
        ..Default::default()
    })
    .unwrap();
    assert!(credit::decode(&GOLDEN, parent(), &small, D::ViewerToHost, T::Reliable).is_err());
    assert!(
        Accepted {
            byte_allowance: 4096,
            ..accepted()
        }
        .negotiate(original, parent(), &small)
        .is_err()
    );
}
fn offer(native: bool, credit: Option<u16>, role: Role) -> Offer {
    let mut capabilities = Vec::new();
    if native {
        capabilities.push(Capability {
            name: attachment::CAPABILITY.into(),
            version: attachment::VERSION,
            required: true,
        });
    }
    if let Some(version) = credit {
        capabilities.push(Capability {
            name: credit::CAPABILITY.into(),
            version,
            required: false,
        });
    }
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role,
        limits: L,
        capabilities,
    }
}
#[test]
fn bilateral_version_selection_and_native_profile_are_required_in_both_session_roles() {
    for role in [Role::Observe, Role::RequestControl] {
        for host in [None, Some(1), Some(2)] {
            for client in [None, Some(1), Some(2)] {
                let selected = offer(true, host, Role::Observe)
                    .intersect(&offer(true, client, role))
                    .unwrap()
                    .select()
                    .unwrap();
                assert_eq!(
                    credit::selected(&selected),
                    host == Some(1) && client == Some(1)
                );
            }
        }
        assert!(!credit::selected(
            &offer(false, Some(1), role).select().unwrap()
        ));
        let mut invalid = offer(true, Some(1), role).select().unwrap();
        invalid.capabilities.push(invalid.capabilities[0].clone());
        assert!(!credit::selected(&invalid));
    }
}
