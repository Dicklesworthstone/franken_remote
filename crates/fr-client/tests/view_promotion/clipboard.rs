//! Decoder/visibility events and wire handoff are explicit deterministic test
//! fixtures. The actual receiver, grant decoder and original owners are real.
use super::*;
use fr_client::{
    clipboard::{ControllerClipboard, Error as ClipboardError},
    control_grant::RequestControl,
};
use fr_wire::{
    clipboard::session::{Admission, Pump, RecordSink, TransportFailure},
    control::{self, Granted, Request, Target},
    negotiation::ControlBinding,
};
fn accepted() -> InputClient {
    let c = credentials();
    let g = Granted {
        request: Request {
            parent: ControlBinding {
                id: 5,
                host_boot: HostBootId::from_raw(4),
                os_session: OsSessionId::from_raw(6),
                remote_session: c.session,
            },
            sequence: 1,
            target: Target {
                display_binding: 1,
                view: c.view,
                bounds: InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
                capabilities: Capabilities::default().with(Capability::Keys),
            },
        },
        input_channel: 7,
        lease: c.lease,
        ticket: c.ticket,
        issued_at_us: 1_000_000,
        lease_until_us: 3_000_000,
        ticket_until_us: 1_500_000,
        first_action: 0,
        first_pointer: 0,
    };
    let mut r = RequestControl::new(
        g.request,
        7,
        ProtocolLimits::ABSOLUTE,
        ClientInstant(10_000),
    )
    .unwrap();
    r.sent(ClientInstant(10_000)).unwrap();
    let mut bytes = [0; control::GRANTED_BYTES];
    control::encode_granted(
        g,
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    r.accept(&bytes, clock(), Policy::default(), ClientInstant(30_000))
        .unwrap()
        .1
}
fn attached() -> (ReceivePipeline, PresentedInput, ControllerClipboard) {
    let (receiver, view) = shown(true);
    let mut input =
        PresentedInput::from_view(accepted(), &receiver, view, ClientInstant(30_000)).unwrap();
    input
        .confirm_mapping(
            credentials().session,
            credentials().view,
            ClientInstant(30_000),
        )
        .unwrap();
    let parent = ControlBinding {
        id: 5,
        host_boot: HostBootId::from_raw(4),
        os_session: OsSessionId::from_raw(6),
        remote_session: credentials().session,
    };
    let outgoing = fr_wire::clipboard::Context {
        scope: fr_core::clipboard::Binding {
            session: credentials().session,
            lease: credentials().lease,
        },
        channel: 77,
        sender: fr_wire::clipboard::Role::Controller,
        lane: fr_wire::clipboard::Lane::Clipboard,
    };
    let clipboard = input
        .attach_clipboard_lane(
            parent,
            outgoing,
            ProtocolLimits::ABSOLUTE,
            true,
            ClientInstant(30_000),
        )
        .unwrap();
    (receiver, input, clipboard)
}
#[derive(Default)]
struct Gate(usize);
impl RecordSink for Gate {
    fn try_send(&mut self, _: &[u8]) -> Result<Admission, TransportFailure> {
        self.0 += 1;
        Ok(Admission::Accepted)
    }
}
#[test]
fn original_receiver_close_or_drop_fences_clipboard_before_presented_input_ticks() {
    for drop_receiver in [true, false] {
        let (mut receiver, mut input, mut clipboard) = attached();
        clipboard
            .offer(1, "never emitted", None, ClientInstant(30_000))
            .unwrap();
        let (_replacement, _) = super::receiver();
        if drop_receiver {
            drop(receiver);
        } else {
            receiver.close();
        }
        let mut gate = Gate::default();
        let mut scratch = [0; 1024];
        assert!(
            clipboard
                .pump(&mut scratch, &mut gate, || ClientInstant(30_001))
                .is_err()
        );
        assert!(clipboard.is_closed());
        assert_eq!(clipboard.retained_bytes(), 0);
        assert_eq!(gate.0, 0);
        assert!(input.tick(ClientInstant(30_001)).is_err());
    }
}
#[test]
fn closing_equal_id_foreign_receiver_does_not_revoke_original_clipboard() {
    let (_receiver, mut input, mut clipboard) = attached();
    let (mut foreign, _) = super::receiver();
    foreign.close();
    drop(foreign);
    clipboard
        .offer(1, "original still live", None, ClientInstant(30_000))
        .unwrap();
    assert_eq!(
        clipboard.pump(&mut [0; 1024], &mut Gate::default(), || ClientInstant(
            30_000
        )),
        Ok(Pump::RecordAccepted)
    );
    input.hidden();
    assert_eq!(
        clipboard.offer(2, "hidden", None, ClientInstant(30_000)),
        Err(ClipboardError::Stopped)
    );
}
#[test]
fn presented_input_does_not_upgrade_opaque_legacy_input_or_implicit_permission() {
    let (receiver, view) = shown(true);
    let mut legacy =
        PresentedInput::from_view(input(credentials()), &receiver, view, ClientInstant(30_000))
            .unwrap();
    legacy
        .confirm_mapping(
            credentials().session,
            credentials().view,
            ClientInstant(30_000),
        )
        .unwrap();
    assert_eq!(
        legacy
            .attach_clipboard(77, true, ClientInstant(30_000))
            .unwrap_err(),
        Error::Clipboard(ClipboardError::NotGranted)
    );
    let (receiver, view) = shown(true);
    let mut input =
        PresentedInput::from_view(accepted(), &receiver, view, ClientInstant(30_000)).unwrap();
    input
        .confirm_mapping(
            credentials().session,
            credentials().view,
            ClientInstant(30_000),
        )
        .unwrap();
    assert_eq!(
        input
            .attach_clipboard(77, false, ClientInstant(30_000))
            .unwrap_err(),
        Error::Clipboard(ClipboardError::Permission)
    );
    assert!(
        input
            .attach_clipboard(77, true, ClientInstant(30_000))
            .is_ok()
    );
}
