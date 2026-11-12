//! Real accepted grants, owner clocks and wire records; native publication is
//! an explicit recording fixture. Lock tests hold the actual projection mutexes.
use super::*;
use crate::{
    clipboard::ControllerClipboard,
    control_grant::RequestControl,
    input::{Action, InputClient, Policy, PresentedObservation},
};
use fr_core::{
    clipboard::{ClipboardSink, Endpoint, Error as CoreError, PlatformError, Publication, Stamp},
    ids::*,
    input::*,
    input_submission::{Capabilities, Capability},
    limits::ProtocolLimits,
};
use fr_media::freshness::{ClockPolicy, ClockSample};
use fr_wire::{
    clipboard::{Body, Context, Lane, Message, Role, session::SessionError},
    control::{self, Granted, Request, Target},
    input::{InputDelivery as T, InputDirection as D},
};
use std::{sync::mpsc, time::Duration};

const L: ProtocolLimits = ProtocolLimits::ABSOLUTE;
const H: u64 = 1_000_000;
const C: u64 = 10_000_000_000;
fn client(elapsed: u64) -> ClientInstant {
    ClientInstant(C + 100 + elapsed)
}
fn host_at(elapsed: u64) -> HostInstant {
    HostInstant::from_micros(H + 100 + elapsed)
}
fn grant() -> Granted {
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
fn policy() -> Policy {
    Policy {
        view_age_us: 1_500_000,
        receipt_timeout_us: 500_000,
    }
}
fn correlation(elapsed: u64) -> ClockCorrelation {
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
fn accepted() -> InputClient {
    accepted_with_limits(L)
}
fn accepted_with_limits(limits: ProtocolLimits) -> InputClient {
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
fn ready(input: &mut InputClient, serial: u64, elapsed: u64) {
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
fn attached() -> (InputClient, ControllerClipboard) {
    let mut input = accepted();
    ready(&mut input, 0, 0);
    let lane = input.attach_clipboard(77, true, client(0)).unwrap();
    lane.local_switch().set_enabled(true);
    lane.peer_switch().set_enabled(true);
    (input, lane)
}
fn ticket(sequence: u64, issued_at_us: u64, expires_at_us: u64) -> Vec<u8> {
    let mut t = grant().initial_ticket();
    t.sequence = sequence;
    t.credentials.ticket = InputTicketId::from_raw(100 + u128::from(sequence));
    t.issued_at_us = issued_at_us;
    t.expires_at_us = expires_at_us;
    let mut b = vec![0; fr_wire::input_ticket::INPUT_TICKET_BYTES];
    fr_wire::input_ticket::encode(t, &mut b, &L, 9, D::HostToViewer, T::Reliable).unwrap();
    b
}
fn context(role: Role) -> Context {
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

#[derive(Default)]
struct Platform {
    text: Vec<String>,
    prepared: usize,
    cancelled: usize,
    until: Option<HostInstant>,
    outcome: Option<Publication>,
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
    fn publish_until(&mut self, text: &str, stamp: Stamp, until: HostInstant) -> Publication {
        self.until = Some(until);
        self.publish(text, stamp)
    }
    fn cancel_prepared(&mut self) {
        self.cancelled += 1;
    }
}

fn stage(
    lane: &mut ControllerClipboard,
    sink: &mut Platform,
    sequence: u64,
    text: &str,
    elapsed: u64,
) -> Vec<u8> {
    let total_bytes = u32::try_from(text.len()).unwrap();
    let stamp = Stamp {
        id: u128::from(sequence),
        source: Endpoint::Host,
        sequence,
    };
    let encode = |body| {
        let mut bytes = vec![0; 1024];
        let len = fr_wire::clipboard::encode(
            Message { stamp, body },
            context(Role::Host),
            &L,
            &mut bytes,
        )
        .unwrap();
        bytes.truncate(len);
        bytes
    };
    for body in [
        Body::Begin {
            total_bytes,
            chunks: 1,
        },
        Body::Chunk {
            index: 0,
            offset: 0,
            bytes: text.as_bytes(),
        },
    ] {
        assert_eq!(
            lane.receive(&encode(body), sink, || client(elapsed)),
            Ok(None)
        );
    }
    encode(Body::Commit { total_bytes })
}

fn projection(lane: &ControllerClipboard) -> Projection {
    Projection {
        state: lane.clock.state.clone(),
        cursor: lane.clock.cursor.clone(),
        binding: lane.clock.state.upgrade().unwrap().binding,
    }
}

fn press(input: &mut InputClient, elapsed: u64) {
    input
        .action(
            Action::Key {
                key: PhysicalKey::new(4).unwrap(),
                transition: KeyTransition::Press,
            },
            &mut [0; 256],
            client(elapsed),
        )
        .unwrap();
}

#[test]
fn publication_caps_view_and_receipts_without_shortening_the_item_lease() {
    let (mut input, lane) = attached();
    let authority = projection(&lane);
    let at = lane.clock.sample(client(0)).unwrap();
    assert_eq!(authority.deadline(at), Ok(host_at(1_500_000)));
    assert_eq!(
        authority.item_deadline(at),
        Ok(HostInstant::from_micros(grant().lease_until_us))
    );
    press(&mut input, 0);
    let at = lane.clock.sample(client(0)).unwrap();
    assert_eq!(authority.deadline(at), Ok(host_at(500_000)));
    assert_eq!(
        authority.item_deadline(at),
        Ok(HostInstant::from_micros(grant().lease_until_us))
    );
}

#[test]
fn final_clock_at_the_exact_view_deadline_cancels_prepared_text() {
    let (_input, mut lane) = attached();
    let mut sink = Platform::default();
    let commit = stage(&mut lane, &mut sink, 1, "expired view", 0);
    let mut samples = 0;
    let result = lane.receive(&commit, &mut sink, || {
        samples += 1;
        client(if samples == 3 { 1_500_000 } else { 0 })
    });
    assert_eq!(samples, 3);
    assert_eq!(
        result,
        Err(Error::Session(SessionError::Clipboard(CoreError::Expired)))
    );
    assert_eq!(sink.prepared, 1);
    assert_eq!(sink.cancelled, 1);
    assert_eq!(sink.text, [] as [String; 0]);
    assert!(sink.until.is_none());
    assert!(!lane.is_closed());
    assert_eq!(lane.retained_bytes(), 0);
}

#[test]
fn publication_before_the_view_deadline_preserves_real_receipts_and_the_cap() {
    for outcome in [Publication::SubmittedToOs, Publication::UnknownEffect] {
        let (_input, mut lane) = attached();
        let mut sink = Platform {
            outcome: Some(outcome),
            ..Platform::default()
        };
        let commit = stage(&mut lane, &mut sink, 1, "current view", 0);
        let mut samples = 0;
        let receipt = lane
            .receive(&commit, &mut sink, || {
                samples += 1;
                client(if samples == 3 { 1_499_999 } else { 0 })
            })
            .unwrap()
            .unwrap();
        assert_eq!(samples, 3);
        assert_eq!(receipt.publication, outcome);
        assert_eq!(sink.until, Some(host_at(1_500_000)));
        assert_eq!(sink.text, ["current view"]);
        assert_eq!(sink.prepared, 1);
        assert_eq!(sink.cancelled, 1);
        // A duplicate keeps the original uncertain/successful receipt and
        // never retries an external publication.
        assert_eq!(
            lane.receive(&commit, &mut sink, || client(1_499_999)),
            Ok(Some(receipt))
        );
        assert_eq!(sink.text.len(), 1);
    }
}

#[test]
fn a_fresh_view_during_the_final_clock_does_not_replace_the_retained_cap() {
    let (mut input, mut lane) = attached();
    let mut sink = Platform::default();
    let commit = stage(&mut lane, &mut sink, 1, "old attempt", 0);
    let mut samples = 0;
    let result = lane.receive(&commit, &mut sink, || {
        samples += 1;
        if samples == 3 {
            ready(&mut input, 1, 1_400_000);
            client(1_500_000)
        } else {
            client(0)
        }
    });
    assert_eq!(samples, 3);
    assert_eq!(
        result,
        Err(Error::Session(SessionError::Clipboard(CoreError::Expired)))
    );
    assert_eq!(sink.text, [] as [String; 0]);
    assert!(!lane.is_closed());
    // The same original owner can authorize a genuinely new transfer.
    let next = stage(&mut lane, &mut sink, 2, "new attempt", 1_500_001);
    assert_eq!(
        lane.receive(&next, &mut sink, || client(1_500_001))
            .unwrap()
            .unwrap()
            .publication,
        Publication::SubmittedToOs
    );
    assert_eq!(sink.text, ["new attempt"]);
}

#[test]
fn final_view_suspension_refuses_but_the_same_original_owner_can_resume() {
    let (mut input, mut lane) = attached();
    let mut sink = Platform::default();
    let commit = stage(&mut lane, &mut sink, 1, "lost view", 0);
    let mut samples = 0;
    let result = lane.receive(&commit, &mut sink, || {
        samples += 1;
        if samples == 3 {
            input.suspend(client(1));
        }
        client(1)
    });
    assert_eq!(samples, 3);
    assert_eq!(
        result,
        Err(Error::Session(SessionError::Clipboard(
            CoreError::Suspended
        )))
    );
    assert_eq!(sink.prepared, 1);
    assert_eq!(sink.cancelled, 1);
    assert_eq!(sink.text, [] as [String; 0]);
    assert!(!lane.is_closed());

    ready(&mut input, 1, 1_000);
    input
        .accept_ticket(
            &ticket(1, H + 1_000, H + 1_001_000),
            correlation(1_000),
            client(1_000),
        )
        .unwrap();
    assert_eq!(input.suspended_since(), None);
    press(&mut input, 1_000);
    assert_eq!(
        lane.receive(&commit, &mut sink, || client(1_000)),
        Err(Error::Session(SessionError::Clipboard(
            CoreError::UnknownTransfer
        )))
    );
    let next = stage(&mut lane, &mut sink, 2, "resumed owner", 1_000);
    assert_eq!(
        lane.receive(&next, &mut sink, || client(1_000))
            .unwrap()
            .unwrap()
            .publication,
        Publication::SubmittedToOs
    );
    assert_eq!(sink.text, ["resumed owner"]);
}

#[test]
fn the_final_clock_discovers_receiver_recovery_before_native_publication() {
    use fr_media::delivery::{
        MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig, ReceivePipeline, ReceivePolicy,
    };
    let (input, mut lane) = attached();
    let limits = fr_wire::MediaLimits::new(L, 1150, 16384, 64).unwrap();
    let view = grant().request.target.view;
    let mut receiver = ReceivePipeline::new(
        ReceiveConfig {
            limits,
            bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
            epoch: MediaEpoch {
                configuration: view.configuration,
                recovery: view.recovery,
            },
            policy: ReceivePolicy::default(),
        },
        MediaBudget::new(limits.protocol()).unwrap(),
    )
    .unwrap();
    receiver.decoder_configured(client(0).0).unwrap();
    let owner = input.clipboard_projection.as_ref().unwrap();
    owner.bind_receiver(receiver.receiver_lifetime());
    owner.expect_reference_recovery();
    let mut sink = Platform::default();
    let commit = stage(&mut lane, &mut sink, 1, "old receiver epoch", 0);
    let mut samples = 0;
    let result = lane.receive(&commit, &mut sink, || {
        samples += 1;
        if samples == 3 {
            receiver
                .replace(
                    MediaEpoch {
                        configuration: view.configuration,
                        recovery: view.recovery.next().unwrap(),
                    },
                    MediaBindings::new(5, 6, 7, 8).unwrap(),
                    client(1).0,
                )
                .unwrap();
        }
        client(1)
    });
    assert_eq!(samples, 3);
    assert_eq!(
        result,
        Err(Error::Session(SessionError::Clipboard(
            CoreError::Suspended
        )))
    );
    assert_eq!(sink.text, [] as [String; 0]);
    assert_eq!(sink.cancelled, 1);
    assert!(!lane.is_closed());
    assert!(!owner.state.stopped.load(Ordering::Acquire));
    assert!(owner.state.inner.lock().unwrap().paused_since.is_some());
}

#[test]
fn an_incoming_item_survives_a_view_lapse_with_its_original_lease_bound() {
    let (mut input, mut lane) = attached();
    let mut sink = Platform::default();
    let commit = stage(&mut lane, &mut sink, 1, "same bounded transfer", 0);
    input.suspend(client(500_000));
    ready(&mut input, 1, 1_600_000);
    input
        .accept_ticket(
            &ticket(1, H + 1_600_000, H + 2_600_000),
            correlation(1_600_000),
            client(1_600_000),
        )
        .unwrap();
    assert_eq!(input.suspended_since(), None);
    assert_eq!(
        lane.receive(&commit, &mut sink, || client(1_600_000))
            .unwrap()
            .unwrap()
            .publication,
        Publication::SubmittedToOs
    );
    assert_eq!(sink.text, ["same bounded transfer"]);
    assert_eq!(
        sink.until,
        Some(HostInstant::from_micros(grant().lease_until_us))
    );
}

#[test]
fn final_native_clock_never_waits_on_policy_mutexes_or_revokes_input() {
    for hold_cursor in [true, false] {
        let (mut input, mut lane) = attached();
        let mut sink = Platform::default();
        let commit = stage(&mut lane, &mut sink, 1, "must not publish", 0);
        let cursor = lane.clock.cursor.clone();
        let state = lane.clock.state.upgrade().unwrap();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (go_tx, go_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let mut samples = 0;
            let result = lane.receive(&commit, &mut sink, || {
                samples += 1;
                if samples == 3 {
                    ready_tx.send(()).unwrap();
                    go_rx.recv().unwrap();
                }
                client(1)
            });
            done_tx
                .send((result, sink, lane.is_closed(), samples))
                .unwrap();
        });
        let reached_final_sample = ready_rx.recv_timeout(Duration::from_secs(5));
        let cursor_guard =
            (reached_final_sample.is_ok() && hold_cursor).then(|| cursor.lock().unwrap());
        let state_guard =
            (reached_final_sample.is_ok() && !hold_cursor).then(|| state.inner.lock().unwrap());
        let _ = go_tx.send(());
        let completed = done_rx.recv_timeout(Duration::from_secs(5));
        // Release before asserting so a regression cannot strand its worker.
        drop(cursor_guard);
        drop(state_guard);
        worker.join().unwrap();
        reached_final_sample.unwrap();
        let (result, sink, closed, samples) = completed.unwrap();
        assert_eq!(samples, 3);
        assert_eq!(result, Err(Error::Stopped));
        assert!(closed);
        assert_eq!(sink.prepared, 1);
        assert_eq!(sink.cancelled, 1);
        assert_eq!(sink.text, [] as [String; 0]);
        // Input retires its failed optional clipboard projection; its original
        // grant, key state and independently owned auxiliary lanes survive.
        input.tick(client(2)).unwrap();
        assert_eq!(input.stopped(), None);
        press(&mut input, 2);
        assert_eq!(input.pending_actions(), 1);
    }
}

#[test]
fn final_readiness_hook_never_waits_or_grants_a_new_sample() {
    for hold_cursor in [true, false] {
        let (mut input, lane) = attached();
        let authority = Arc::new(projection(&lane));
        let at = lane.clock.sample(client(1)).unwrap();
        let cursor = lane.clock.cursor.clone();
        let state = lane.clock.state.upgrade().unwrap();
        let cursor_guard = hold_cursor.then(|| cursor.lock().unwrap());
        let state_guard = (!hold_cursor).then(|| state.inner.lock().unwrap());
        let other = authority.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            tx.send(other.publication_refusal(at)).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(5));
        drop(cursor_guard);
        drop(state_guard);
        worker.join().unwrap();
        assert_eq!(result.unwrap(), Some(Refusal::AuthorityUnavailable));
        assert!(!authority.is_revoked());
        assert_eq!(authority.publication_refusal(at), None);
        press(&mut input, 2);
    }
}

#[test]
fn owner_progress_after_the_final_sample_cannot_refresh_its_old_cap() {
    let (mut input, lane) = attached();
    let authority = projection(&lane);
    let at = lane.clock.sample(client(0)).unwrap();
    assert_eq!(authority.publication_refusal(at), None);
    ready(&mut input, 1, 1_400_000);
    assert_eq!(
        authority.publication_refusal(at),
        Some(Refusal::AuthorityUnavailable)
    );
    assert!(!authority.is_revoked());
    // A later operation gets its own genuine sample from the same cursor.
    let fresh = lane.clock.sample(client(1_400_001)).unwrap();
    assert_eq!(authority.publication_refusal(fresh), None);
}

#[test]
fn the_native_transport_sample_refuses_contention_without_waiting_or_revoking_input() {
    for hold_cursor in [true, false] {
        let (mut input, lane) = attached();
        let network = lane.transport();
        let cursor = network.clock.cursor.clone();
        let state = network.clock.state.upgrade().unwrap();
        let cursor_guard = hold_cursor.then(|| cursor.lock().unwrap());
        let state_guard = (!hold_cursor).then(|| state.inner.lock().unwrap());
        let (tx, rx) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            tx.send(network.sample_native(client(1))).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(5));
        drop(cursor_guard);
        drop(state_guard);
        worker.join().unwrap();
        assert_eq!(result.unwrap(), Err(Error::Stopped));
        input.tick(client(2)).unwrap();
        assert_eq!(input.stopped(), None);
        press(&mut input, 2);
    }
}

#[test]
fn the_native_transport_sample_keeps_view_suspension_resumable() {
    let (mut input, lane) = attached();
    let network = lane.transport();
    assert_eq!(network.sample_native(client(0)), Ok(host_at(0)));
    input.suspend(client(1));
    assert_eq!(network.sample_native(client(1)), Err(Error::NotReady));
    assert!(
        !network
            .clock
            .state
            .upgrade()
            .unwrap()
            .stopped
            .load(Ordering::Acquire)
    );
    ready(&mut input, 1, 1_000);
    input
        .accept_ticket(
            &ticket(1, H + 1_000, H + 1_001_000),
            correlation(1_000),
            client(1_000),
        )
        .unwrap();
    assert_eq!(network.sample_native(client(1_000)), Ok(host_at(1_000)));
    press(&mut input, 1_000);
}
