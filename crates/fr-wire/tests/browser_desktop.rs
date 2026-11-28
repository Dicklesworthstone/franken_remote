use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, OsSessionId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    limits::ProtocolLimits,
};
use fr_wire::{
    HEADER_BYTES, WireError,
    input::{
        InputDelivery::Reliable,
        InputDirection::{HostToViewer, ViewerToHost},
    },
    negotiation::{
        self, ControlBinding, Message,
        browser::{self, attachment},
    },
};
fn binding() -> ControlBinding {
    ControlBinding {
        id: 1,
        host_boot: HostBootId::from_raw(11),
        os_session: OsSessionId::from_raw(12),
        remote_session: RemoteSessionId::from_raw(13),
    }
}
fn grant() -> attachment::Grant {
    attachment::Grant {
        descriptor: attachment::Descriptor {
            binding: fr_wire::decoder::Binding {
                parent: ControlBinding { id: 2, ..binding() },
                display: 14,
                geometry: DisplayGeometryGeneration::INITIAL,
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
                viewport: ViewportMappingGeneration::INITIAL,
            },
        },
        ticket: attachment::Ticket([9; 32]),
        deadline_us: 20_000,
        byte_allowance: browser::MAX_VIDEO_CREDIT_BYTES,
        picture_allowance: browser::MAX_VIDEO_CREDIT_FRAMES,
        credit_epoch: 1,
    }
}
#[test]
fn browser_negotiation_is_complete_without_relaxing_native_profile() {
    let client = browser::offer(ProtocolLimits::ABSOLUTE);
    let host = browser::intersect(&client, &client).unwrap();
    browser::check_host(&client, &host).unwrap();
    let selection = browser::select(&host).unwrap();
    browser::check_selection(&selection, &host).unwrap();
    assert_eq!(client.validate(), Err(negotiation::Error::Profile));
    assert_eq!(selection.validate(), Err(negotiation::Error::Profile));
    for message in [
        Message::HostCapabilities(host),
        Message::SelectedConfiguration(selection.clone()),
        Message::SessionOpened {
            binding: binding(),
            selection,
            observation_until_us: 50_000,
        },
        Message::BindingAccepted { binding: 1 },
    ] {
        let mut out = [0; browser::MAX_RECORD];
        let length = browser::encode(&message, out.len(), &mut out).unwrap();
        assert_eq!(
            browser::decode(&out[..length], out.len(), message.binding()).unwrap(),
            message
        );
        if !matches!(message, Message::BindingAccepted { .. }) {
            assert_eq!(
                negotiation::decode(&out[..length], out.len(), 0),
                Err(negotiation::Error::Profile)
            );
        }
    }
    let hello = Message::ClientHello(client);
    assert_eq!(
        browser::encode(&hello, browser::MAX_RECORD, &mut [0; browser::MAX_RECORD]),
        Err(negotiation::Error::Profile)
    );
}
#[test]
fn browser_selection_cannot_remove_required_media_or_change_role() {
    let client = browser::offer(ProtocolLimits::ABSOLUTE);
    let mut host = client.clone();
    host.capabilities
        .retain(|capability| capability.name != browser::VIDEO_CAPABILITY);
    assert_eq!(
        browser::check_host(&client, &host),
        Err(negotiation::Error::RequiredCapability)
    );
    let mut selected = browser::select(&client).unwrap();
    selected.role = negotiation::Role::RequestControl;
    assert_eq!(
        browser::check_selection(&selected, &client),
        Err(negotiation::Error::Selection)
    );
}
#[test]
fn browser_attachment_preserves_all_ticket_bytes_and_exact_owner() {
    let g = grant();
    let limits = browser::limits();
    for (message, sender, channel) in [
        (attachment::Message::Binding(g.descriptor), HostToViewer, 1),
        (attachment::Message::Accepted(2), ViewerToHost, 1),
        (attachment::Message::Ticket(g), HostToViewer, 1),
        (attachment::Message::Attach(g), ViewerToHost, 2),
        (attachment::Message::Attached(g), HostToViewer, 2),
    ] {
        let mut out = [0; attachment::GRANT_RECORD_BYTES];
        let n =
            attachment::encode(message, binding(), &limits, &mut out, sender, Reliable).unwrap();
        assert_eq!(
            attachment::decode(&out[..n], binding(), channel, &limits, sender, Reliable).unwrap(),
            message
        );
        if !matches!(message, attachment::Message::Accepted(_)) {
            assert!(
                attachment::decode(
                    &out[..n],
                    ControlBinding {
                        remote_session: RemoteSessionId::from_raw(99),
                        ..binding()
                    },
                    channel,
                    &limits,
                    sender,
                    Reliable
                )
                .is_err()
            );
        }
        let wrong = if sender == ViewerToHost {
            HostToViewer
        } else {
            ViewerToHost
        };
        assert_eq!(
            attachment::decode(&out[..n], binding(), channel, &limits, wrong, Reliable),
            Err(WireError::WrongRole)
        );
        if matches!(message, attachment::Message::Attach(_)) {
            assert!(
                fr_wire::attachment::decode(
                    &out[..n],
                    binding(),
                    channel,
                    &limits,
                    sender,
                    Reliable
                )
                .is_err()
            );
            assert_eq!(n, attachment::GRANT_RECORD_BYTES);
            assert_eq!(&out[HEADER_BYTES + 102..HEADER_BYTES + 134], &[9; 32]);
        }
    }
}
#[test]
fn whole_picture_credit_includes_handoff_copies_and_metadata() {
    assert_eq!(
        browser::frame_charge(browser::MAX_ACCESS_UNIT_BYTES).unwrap(),
        browser::MAX_VIDEO_CREDIT_BYTES - 1
    );
    assert!(browser::frame_charge(0).is_err());
    assert!(browser::frame_charge(browser::MAX_ACCESS_UNIT_BYTES + 1).is_err());
    let mut g = grant();
    g.byte_allowance = browser::frame_charge(browser::MAX_ACCESS_UNIT_BYTES).unwrap() - 1;
    assert!(g.validate(binding(), &browser::limits()).is_err());
    g = grant();
    g.ticket.0 = [0; 32];
    assert!(g.validate(binding(), &browser::limits()).is_err());
    g = grant();
    g.picture_allowance = browser::MAX_VIDEO_CREDIT_FRAMES + 1;
    assert!(g.validate(binding(), &browser::limits()).is_err());
}
#[test]
fn mixed_role_inspection_preserves_strict_channel_decoders() {
    let limits = fr_wire::MediaLimits::new(
        browser::limits(),
        browser::MAX_VIDEO_RECORD_BYTES,
        fr_wire::MAX_FRAGMENTS,
        fr_wire::MAX_REPAIR_RANGES,
    )
    .unwrap();
    let progress = fr_wire::Progress {
        descriptor: fr_wire::FrameDescriptor {
            frame: 1,
            total_bytes: 100,
            stride: 100,
            capture_micros: 400,
            reference: None,
        },
        observed_micros: 400,
        observation: fr_wire::SourceObservation::Captured,
        pipeline: fr_wire::PipelineState::Running,
    };
    let mut out = [0; 128];
    let n = fr_wire::encode_progress(progress, 2, &limits, &mut out).unwrap();
    assert_eq!(
        browser::record_kind(&out[..n], 128, 2).unwrap(),
        fr_wire::Kind::Progress
    );
    assert_eq!(
        browser::record_kind(&out[..n], n - 1, 2),
        Err(WireError::ResourceLimit)
    );
    assert_eq!(
        fr_wire::Record::decode(&out[..n], &limits, 2, fr_wire::Channel::Control).unwrap_err(),
        WireError::WrongChannel
    );
}
