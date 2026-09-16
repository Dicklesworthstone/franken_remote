//! Explicit grant/presentation fixtures. This is NOT live tailnet qualification.
use super::{attachment::parent, transport_support::clock};
use asupersync::cx::Cx;
use fr_client::{
    control_grant::RequestControl,
    input::{ClientInstant, InputClient, Policy, PresentedObservation},
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_submission::{Capabilities, Capability, InputSession},
    limits::ProtocolLimits,
    time::{HostDuration, HostInstant},
};
use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample};
use fr_wire::{
    control::{self, Granted, Request, Target},
    input::{InputDelivery, InputDirection},
};
pub fn grant(cx: &Cx) -> Granted {
    let t = clock(cx);
    Granted {
        request: Request {
            parent: parent(),
            sequence: 1,
            target: Target {
                display_binding: 8,
                view: InputView {
                    geometry: DisplayGeometryGeneration::INITIAL,
                    viewport: ViewportMappingGeneration::INITIAL,
                    configuration: CodecConfigurationGeneration::INITIAL,
                    recovery: RecoveryGeneration::INITIAL,
                },
                bounds: InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
                capabilities: Capabilities::default().with(Capability::Keys),
            },
        },
        input_channel: 9,
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        issued_at_us: t,
        lease_until_us: t + 3_000_000,
        ticket_until_us: t + 1_000_000,
        first_action: 0,
        first_pointer: 0,
    }
}
pub fn host(g: Granted) -> InputSession {
    let t = HostInstant::from_micros(g.issued_at_us);
    let mut a = SessionAuthority::new(
        g.request.parent.remote_session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(10_000_000),
            ticket_lifetime: HostDuration::from_micros(1_000_000),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(t).unwrap();
    a.mark_view_ready(t).unwrap();
    a.grant_lease(g.lease, t).unwrap();
    a.issue_input_ticket(g.lease, g.ticket, t).unwrap();
    InputSession::new(
        a,
        g.credentials(),
        g.request.target.bounds,
        g.request.target.capabilities,
        t,
    )
    .unwrap()
}
pub fn viewer(cx: &Cx, g: Granted) -> InputClient {
    let sent = ClientInstant(g.issued_at_us);
    let mut request = RequestControl::new(g.request, 9, ProtocolLimits::ABSOLUTE, sent).unwrap();
    request.sent(sent).unwrap();
    let mut bytes = [0; control::GRANTED_BYTES];
    control::encode_granted(
        g,
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    let received = clock(cx);
    let correlation = ClockCorrelation::new(
        ClockSample {
            host_boot: g.request.parent.host_boot,
            client_sent_us: sent.0,
            client_received_us: received,
            host_sample_us: g.issued_at_us,
        },
        ClockPolicy {
            drift_ppm: 0,
            ..ClockPolicy::default()
        },
    )
    .unwrap();
    let (_, mut input) = request
        .accept(
            &bytes,
            correlation,
            Policy {
                view_age_us: 1_500_000,
                ..Policy::default()
            },
            ClientInstant(received),
        )
        .unwrap();
    input
        .confirm_mapping(
            g.credentials().session,
            g.credentials().view,
            ClientInstant(received),
        )
        .unwrap();
    present(cx, &mut input, g, 1);
    input
}
pub fn present(cx: &Cx, input: &mut InputClient, g: Granted, serial: u64) {
    let t = ClientInstant(clock(cx));
    input
        .presented(
            PresentedObservation {
                session: g.credentials().session,
                view: g.credentials().view,
                serial,
                received_at: t,
                source_age_upper_us: 0,
            },
            t,
        )
        .unwrap();
}
