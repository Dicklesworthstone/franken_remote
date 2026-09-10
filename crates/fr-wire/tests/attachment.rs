use fr_core::{
    ids::*,
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{
    attachment::{self, *},
    decoder::Binding,
    input::{InputDelivery as T, InputDirection as D},
    negotiation::ControlBinding,
    stream::RecordStream,
};
const L: ProtocolLimits = ProtocolLimits::ABSOLUTE;
fn parent() -> ControlBinding {
    ControlBinding {
        id: 7,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    }
}
fn descriptor() -> Descriptor {
    Descriptor {
        binding: Binding {
            parent: ControlBinding { id: 8, ..parent() },
            display: 14,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
        },
        role: MediaRole::Configuration,
        host_stream: 7,
        viewer_stream: 6,
    }
}
fn grant() -> Grant {
    Grant {
        descriptor: descriptor(),
        ticket: Ticket(0x1234_5678_90ab_cdef),
        deadline_us: 900,
        byte_allowance: 4096,
        picture_allowance: 0,
        credit_epoch: 8,
    }
}
fn encoded(m: Message) -> Vec<u8> {
    let mut b = vec![0; GRANT_RECORD_BYTES];
    let n = attachment::encode(
        m,
        parent(),
        &L,
        &mut b,
        if matches!(m, Message::Attach(_) | Message::Accepted(_)) {
            D::ViewerToHost
        } else {
            D::HostToViewer
        },
        T::Reliable,
    )
    .unwrap();
    b.truncate(n);
    b
}
fn header(kind: u16, len: usize, binding: u32) -> Vec<u8> {
    let mut b = b"FRD0\0\0".to_vec();
    b.extend(kind.to_be_bytes());
    b.extend([0; 4]);
    b.extend(u32::try_from(len).unwrap().to_be_bytes());
    b.extend(binding.to_be_bytes());
    b.extend([0; 4]);
    b
}
#[test]
fn independent_binding_and_ticket_golden_bytes() {
    let mut ack = header(0x1c, 4, 7);
    ack.extend(8u32.to_be_bytes());
    assert_eq!(encoded(Message::Accepted(8)), ack);
    let mut d = 8u32.to_be_bytes().to_vec();
    for n in [11u128, 12, 13, 14] {
        d.extend(n.to_be_bytes());
    }
    d.extend([0; 32]);
    d.extend([1, 1]);
    d.extend(7u64.to_be_bytes());
    d.extend(6u64.to_be_bytes());
    let mut expected = header(0x1b, 118, 7);
    expected.extend(&d);
    assert_eq!(encoded(Message::Binding(descriptor())), expected);
    d.extend(0x1234_5678_90ab_cdefu128.to_be_bytes());
    d.extend(900u64.to_be_bytes());
    d.extend(4096u64.to_be_bytes());
    d.extend(0u32.to_be_bytes());
    d.extend(8u64.to_be_bytes());
    for (kind, channel, m) in [
        (0x18, 7, Message::Ticket(grant())),
        (0x19, 8, Message::Attach(grant())),
        (0x1a, 8, Message::Attached(grant())),
    ] {
        let mut expected = header(kind, 162, channel);
        expected.extend(&d);
        assert_eq!(encoded(m), expected);
    }
}
#[test]
fn all_truncations_trailing_bytes_wrong_directions_and_datagrams_refuse() {
    for m in [
        Message::Accepted(8),
        Message::Binding(descriptor()),
        Message::Ticket(grant()),
        Message::Attach(grant()),
        Message::Attached(grant()),
    ] {
        let bytes = encoded(m);
        let direction = if matches!(m, Message::Attach(_) | Message::Accepted(_)) {
            D::ViewerToHost
        } else {
            D::HostToViewer
        };
        let channel = if matches!(
            m,
            Message::Binding(_) | Message::Ticket(_) | Message::Accepted(_)
        ) {
            7
        } else {
            8
        };
        for n in 0..bytes.len() {
            assert!(
                attachment::decode(&bytes[..n], parent(), channel, &L, direction, T::Reliable)
                    .is_err()
            );
        }
        assert_eq!(
            attachment::decode(&bytes, parent(), channel, &L, direction, T::Reliable).unwrap(),
            m
        );
        assert!(attachment::decode(&bytes, parent(), channel, &L, direction, T::Datagram).is_err());
        let opposite = if direction == D::ViewerToHost {
            D::HostToViewer
        } else {
            D::ViewerToHost
        };
        assert!(attachment::decode(&bytes, parent(), channel, &L, opposite, T::Reliable).is_err());
        let mut longer = bytes;
        longer.push(0);
        assert!(
            attachment::decode(&longer, parent(), channel, &L, direction, T::Reliable).is_err()
        );
    }
}
#[test]
fn every_parent_scope_and_stream_identity_is_checked() {
    let m = Message::Attach(grant());
    let bytes = encoded(m);
    for p in [
        ControlBinding { id: 0, ..parent() },
        ControlBinding {
            host_boot: HostBootId::from_raw(99),
            ..parent()
        },
        ControlBinding {
            os_session: OsSessionId::from_raw(99),
            ..parent()
        },
        ControlBinding {
            remote_session: RemoteSessionId::from_raw(99),
            ..parent()
        },
    ] {
        assert!(attachment::decode(&bytes, p, 8, &L, D::ViewerToHost, T::Reliable).is_err());
    }
    for stream in [0, 2, 6, u64::MAX, (1 << 62) + 3] {
        let d = Descriptor {
            host_stream: stream,
            ..descriptor()
        };
        assert!(d.validate(parent()).is_err());
    }
    for stream in [0, 3, 7, u64::MAX, (1 << 62) + 2] {
        let d = Descriptor {
            viewer_stream: stream,
            ..descriptor()
        };
        assert!(d.validate(parent()).is_err());
    }
}
#[test]
fn budgets_tokens_and_output_capacity_are_checked_without_allocation() {
    for g in [
        Grant {
            ticket: Ticket(0),
            ..grant()
        },
        Grant {
            deadline_us: 0,
            ..grant()
        },
        Grant {
            byte_allowance: 0,
            ..grant()
        },
        Grant {
            byte_allowance: u64::MAX,
            ..grant()
        },
        Grant {
            picture_allowance: 1,
            ..grant()
        },
        Grant {
            credit_epoch: 0,
            ..grant()
        },
    ] {
        assert!(g.validate(parent(), &L).is_err());
    }
    let mut b = [0; GRANT_RECORD_BYTES - 1];
    assert!(
        attachment::encode(
            Message::Ticket(grant()),
            parent(),
            &L,
            &mut b,
            D::HostToViewer,
            T::Reliable
        )
        .is_err()
    );
    let small = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(128),
        ..Default::default()
    })
    .unwrap();
    assert!(
        attachment::decode(
            &encoded(Message::Ticket(grant())),
            parent(),
            7,
            &small,
            D::HostToViewer,
            T::Reliable
        )
        .is_err()
    );
}
#[test]
fn bootstrap_never_accepts_attachment_and_debug_redacts_secrets() {
    let mut bytes = encoded(Message::Ticket(grant()));
    bytes[16..20].fill(0);
    let mut stream = RecordStream::negotiation(4096, 1000).unwrap();
    assert!(stream.push(&bytes, 0).is_err());
    assert_eq!(stream.allocated_bytes(), 0);
    for text in [
        format!("{:?}", grant()),
        format!("{:#?}", Message::Attach(grant())),
        format!("{:?}", grant().ticket),
    ] {
        assert!(!text.contains("1234567890abcdef"));
        assert!(!text.contains(&grant().ticket.0.to_string()));
    }
}
