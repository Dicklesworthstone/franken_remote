use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    clipboard::{ClipboardSession, ClipboardSink, Endpoint, PlatformError, Publication, Stamp},
    ids::*,
    input::*,
    input_submission::{Capabilities, InputSession},
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_wire::{
    WireError,
    clipboard::{
        Context, Lane, Role,
        receive::{ReceiveError, receive},
        send::Sender,
    },
};
fn owner() -> InputSession {
    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut a = SessionAuthority::new(credentials.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(HostInstant::ORIGIN).unwrap();
    a.mark_view_ready(HostInstant::ORIGIN).unwrap();
    a.grant_lease(credentials.lease, HostInstant::ORIGIN)
        .unwrap();
    a.issue_input_ticket(credentials.lease, credentials.ticket, HostInstant::ORIGIN)
        .unwrap();
    InputSession::new(
        a,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        HostInstant::ORIGIN,
    )
    .unwrap()
}
#[derive(Default)]
struct Sink {
    text: Option<String>,
    calls: u32,
}
impl ClipboardSink for Sink {
    fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
        Ok(())
    }
    fn publish(&mut self, text: &str, _: Stamp) -> Publication {
        self.text = Some(text.to_owned());
        self.calls += 1;
        Publication::SubmittedToOs
    }
}
fn parts(input: &InputSession, text: &str) -> (ClipboardSession, Sender, Context) {
    let session = ClipboardSession::new(
        input,
        Endpoint::Host,
        ProtocolLimits::ABSOLUTE,
        true,
        HostInstant::ORIGIN,
    )
    .unwrap();
    let ctx = Context {
        scope: session.binding(),
        channel: 17,
        sender: Role::Controller,
        lane: Lane::Clipboard,
    };
    let sender = Sender::new(
        text,
        Stamp {
            id: 4,
            source: Endpoint::Controller,
            sequence: 1,
        },
        ctx,
        ProtocolLimits::ABSOLUTE,
    )
    .unwrap();
    (session, sender, ctx)
}
#[test]
fn complete_wire_to_final_authority_to_publication_pipeline() {
    for text in [String::new(), "🦀".repeat(262_144)] {
        let input = owner();
        let (mut session, mut sender, ctx) = parts(&input, &text);
        let mut sink = Sink::default();
        let mut out = vec![0; 65_536];
        let mut receipts = 0;
        while let Some(n) = sender.encode_next(&mut out).unwrap() {
            if let Some(receipt) = receive(
                &mut session,
                &out[..n],
                ctx,
                &ProtocolLimits::ABSOLUTE,
                &mut sink,
                || HostInstant::ORIGIN,
            )
            .unwrap()
            {
                assert_eq!(receipt.publication, Publication::SubmittedToOs);
                receipts += 1;
            }
            sender.accepted();
        }
        assert_eq!(sink.text.as_deref(), Some(text.as_str()));
        assert_eq!(sink.calls, 1);
        assert_eq!(receipts, 1);
        assert_eq!(session.reserved_bytes(), 0);
    }
}
#[test]
fn revoke_between_last_chunk_and_commit_never_reaches_native_sink() {
    let mut input = owner();
    let (mut session, mut sender, ctx) = parts(&input, "secret");
    let mut sink = Sink::default();
    let mut out = [0; 256];
    for _ in 0..2 {
        let n = sender.encode_next(&mut out).unwrap().unwrap();
        receive(
            &mut session,
            &out[..n],
            ctx,
            &ProtocolLimits::ABSOLUTE,
            &mut sink,
            || HostInstant::ORIGIN,
        )
        .unwrap();
        sender.accepted();
    }
    input.revoke();
    let n = sender.encode_next(&mut out).unwrap().unwrap();
    assert!(
        receive(
            &mut session,
            &out[..n],
            ctx,
            &ProtocolLimits::ABSOLUTE,
            &mut sink,
            || HostInstant::ORIGIN
        )
        .is_err()
    );
    assert_eq!(sink.calls, 0);
    assert!(session.is_closed());
    assert_eq!(session.reserved_bytes(), 0);
}
#[test]
fn malformed_ordered_record_clears_staging_but_not_input_authority() {
    let input = owner();
    let (mut session, sender, ctx) = parts(&input, "secret");
    let mut sink = Sink::default();
    let mut out = [0; 256];
    let n = sender.encode_next(&mut out).unwrap().unwrap();
    receive(
        &mut session,
        &out[..n],
        ctx,
        &ProtocolLimits::ABSOLUTE,
        &mut sink,
        || HostInstant::ORIGIN,
    )
    .unwrap();
    assert_eq!(session.reserved_bytes(), 6);
    assert_eq!(
        receive(
            &mut session,
            b"BAD!",
            ctx,
            &ProtocolLimits::ABSOLUTE,
            &mut sink,
            || HostInstant::ORIGIN
        ),
        Err(ReceiveError::Wire(WireError::BadMagic))
    );
    assert!(session.is_closed());
    assert_eq!(session.reserved_bytes(), 0);
    assert!(!input.monitor().is_revoked());
    assert_eq!(sink.calls, 0);
}
