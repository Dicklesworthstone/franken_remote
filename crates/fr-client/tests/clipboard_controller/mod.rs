use fr_client::{
    clipboard::ControllerClipboard,
    control_grant::RequestControl,
    input::{ClientInstant, InputClient, Policy, PresentedObservation},
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    clipboard::{Binding, ClipboardSink, PlatformError, Publication, Stamp},
    ids::*,
    input::*,
    input_submission::{Capabilities, Capability, InputSession},
    limits::ProtocolLimits,
    time::{HostDuration, HostInstant},
};
use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample};
use fr_wire::{
    clipboard::{
        Context, Lane, Role,
        session::{Admission, ChannelSession, Pump, RecordSink, TransportFailure},
    },
    control::{self, Granted, Request, Target},
    input::{InputDelivery as T, InputDirection as D},
    negotiation::ControlBinding,
};
pub const L: ProtocolLimits = ProtocolLimits::ABSOLUTE;
pub const H: u64 = 1_000_000;
pub const C: u64 = 10_000_000_000;
pub fn client(elapsed: u64) -> ClientInstant {
    ClientInstant(C + 100 + elapsed)
}
pub fn host_at(elapsed: u64) -> HostInstant {
    HostInstant::from_micros(H + 100 + elapsed)
}
pub fn grant() -> Granted {
    Granted {
        request: Request {
            parent: ControlBinding {
                id: 7,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(3),
            },
            sequence: 9,
            target: Target {
                display_binding: 8,
                view: InputView {
                    geometry: DisplayGeometryGeneration::INITIAL,
                    viewport: ViewportMappingGeneration::INITIAL,
                    configuration: CodecConfigurationGeneration::INITIAL,
                    recovery: RecoveryGeneration::INITIAL,
                },
                bounds: InputBounds::new(DesktopPoint { x: 0, y: 0 }, 100, 100).unwrap(),
                capabilities: Capabilities::default().with(Capability::Keys),
            },
        },
        input_channel: 9,
        lease: InputLeaseId::from_raw(10),
        ticket: InputTicketId::from_raw(11),
        issued_at_us: H,
        lease_until_us: H + 3_000_000,
        ticket_until_us: H + 1_000_000,
        first_action: 0,
        first_pointer: 0,
    }
}
pub fn policy() -> Policy {
    Policy {
        view_age_us: 1_500_000,
        receipt_timeout_us: 500_000,
    }
}
pub fn correlation(elapsed: u64) -> ClockCorrelation {
    ClockCorrelation::new(
        ClockSample {
            host_boot: grant().request.parent.host_boot,
            client_sent_us: C + elapsed,
            client_received_us: C + 100 + elapsed,
            host_sample_us: H + elapsed,
        },
        ClockPolicy {
            drift_ppm: 0,
            ..ClockPolicy::default()
        },
    )
    .unwrap()
}
pub fn accepted() -> InputClient {
    accepted_with_limits(L)
}
pub fn accepted_with_limits(limits: ProtocolLimits) -> InputClient {
    let g = grant();
    let mut request =
        RequestControl::new(g.request, g.input_channel, limits, ClientInstant(C)).unwrap();
    request.sent(ClientInstant(C)).unwrap();
    let mut b = [0; control::GRANTED_BYTES];
    control::encode_granted(g, &mut b, &limits, D::HostToViewer, T::Reliable).unwrap();
    request
        .accept(&b, correlation(0), policy(), client(0))
        .unwrap()
        .1
}
pub fn ready(input: &mut InputClient, serial: u64, elapsed: u64) {
    let g = grant();
    input
        .confirm_mapping(
            g.credentials().session,
            g.credentials().view,
            client(elapsed),
        )
        .unwrap();
    input
        .presented(
            PresentedObservation {
                session: g.credentials().session,
                serial,
                view: g.credentials().view,
                received_at: client(elapsed),
                source_age_upper_us: 0,
            },
            client(elapsed),
        )
        .unwrap();
}
pub fn attached() -> (InputClient, ControllerClipboard) {
    let mut input = accepted();
    ready(&mut input, 0, 0);
    let lane = input.attach_clipboard(77, true, client(0)).unwrap();
    lane.local_switch().set_enabled(true);
    lane.peer_switch().set_enabled(true);
    (input, lane)
}
pub fn ticket(sequence: u64, issued_at_us: u64, expires_at_us: u64) -> Vec<u8> {
    let mut t = grant().initial_ticket();
    t.sequence = sequence;
    t.credentials.ticket = InputTicketId::from_raw(100 + u128::from(sequence));
    t.issued_at_us = issued_at_us;
    t.expires_at_us = expires_at_us;
    let mut b = vec![0; fr_wire::input_ticket::INPUT_TICKET_BYTES];
    fr_wire::input_ticket::encode(t, &mut b, &L, 9, D::HostToViewer, T::Reliable).unwrap();
    b
}
pub fn context(role: Role) -> Context {
    Context {
        scope: Binding {
            session: grant().request.parent.remote_session,
            lease: grant().lease,
        },
        channel: 77,
        sender: role,
        lane: Lane::Clipboard,
    }
}
pub fn host_owner() -> InputSession {
    let g = grant();
    let at = HostInstant::from_micros(H);
    let mut a = SessionAuthority::new(
        g.request.parent.remote_session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(3_000_000),
            ticket_lifetime: HostDuration::from_micros(1_000_000),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(at).unwrap();
    a.mark_view_ready(at).unwrap();
    a.grant_lease(g.lease, at).unwrap();
    a.issue_input_ticket(g.lease, g.ticket, at).unwrap();
    InputSession::new(
        a,
        g.credentials(),
        g.request.target.bounds,
        g.request.target.capabilities,
        at,
    )
    .unwrap()
}
pub fn host_channel(input: &InputSession) -> ChannelSession {
    let mut channel = ChannelSession::new(input, context(Role::Host), L, true, host_at(0)).unwrap();
    channel.set_enabled(true, true);
    channel
}
pub fn scratch() -> Vec<u8> {
    vec![0xa5; 16_384 + 93]
}
#[derive(Default)]
pub struct Platform {
    pub text: Vec<String>,
    pub outcome: Option<Publication>,
    pub prepared: usize,
    pub cancelled: usize,
}
impl ClipboardSink for Platform {
    fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
        self.prepared += 1;
        Ok(())
    }
    fn publish(&mut self, text: &str, _: Stamp) -> Publication {
        self.text.push(text.to_owned());
        self.outcome.unwrap_or(Publication::SubmittedToOs)
    }
    fn cancel_prepared(&mut self) {
        self.cancelled += 1;
    }
}
#[derive(Default)]
pub struct Gate {
    pub last: Vec<u8>,
    pub calls: usize,
    pub blocked: bool,
}
impl RecordSink for Gate {
    fn try_send(&mut self, b: &[u8]) -> Result<Admission, TransportFailure> {
        self.calls += 1;
        self.last = b.to_vec();
        Ok(if self.blocked {
            Admission::Backpressure
        } else {
            Admission::Accepted
        })
    }
}
pub fn incoming(text: &str) -> Vec<Vec<u8>> {
    let input = host_owner();
    let mut from = host_channel(&input);
    from.offer(1, text, None, host_at(0)).unwrap();
    let mut gate = Gate::default();
    let mut records = Vec::new();
    for _ in 0..1030 {
        let p = from.pump(&mut scratch(), &mut gate, || host_at(0)).unwrap();
        records.push(gate.last.clone());
        if matches!(p, Pump::ItemAccepted(_)) {
            return records;
        }
    }
    panic!("bounded fixture should complete");
}
