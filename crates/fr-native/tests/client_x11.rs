#![cfg(all(target_os = "linux", feature = "linux-input-agent"))]
use asupersync::{runtime::RuntimeBuilder, types::Budget};
use fr_client::input::{
    Action, ClientInstant, Error, InputClient, Policy, PresentedObservation, ResultEvent,
    StopReason,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::{Dispatch, InputSession},
    limits::ProtocolLimits,
};
use fr_native::{input::X11Pointer, input_agent::start_x11};
use fr_wire::{
    input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES},
    input_result::*,
};
use frd::{
    input_agent::{Agent, Reply, Route, Seat, Shutdown},
    input_watchdog::{StopReason as HostStop, host_now},
};
use std::{
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct Server {
    child: Child,
    display: String,
}
impl Server {
    fn start() -> Self {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-nolisten",
                "tcp",
                "-noreset",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut number = String::new();
        BufReader::new(child.stdout.take().unwrap().take(16))
            .read_line(&mut number)
            .unwrap();
        Self {
            child,
            display: format!(":{}", number.trim().parse::<u16>().unwrap()),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}
fn binding() -> ResultBinding {
    ResultBinding {
        channel: 7,
        session: credentials().session,
        lease: credentials().lease,
    }
}
fn local(us: u64) -> ClientInstant {
    ClientInstant(1_000_000_000 + us)
}
fn eventually(mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(
            Instant::now() < deadline,
            "private X11 integration timed out"
        );
        thread::sleep(Duration::from_millis(1));
    }
}
fn reply(agent: &mut Agent) -> Reply {
    let mut result = None;
    eventually(|| {
        result = agent.try_reply().unwrap();
        result.is_some()
    });
    result.unwrap()
}
fn receipt(reply: Reply) -> InputResult {
    let Reply::Input(Ok(Dispatch::Completed(receipt))) = reply else {
        panic!("native receipt required")
    };
    InputResult::from_receipt(binding(), SequenceSpace::Action, receipt).unwrap()
}
fn encode_result(result: InputResult) -> [u8; INPUT_RESULT_BYTES] {
    let mut bytes = [0; INPUT_RESULT_BYTES];
    let written = encode_input_result(
        result,
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(written, bytes.len());
    bytes
}
struct Running {
    done: mpsc::Receiver<Shutdown>,
    join: thread::JoinHandle<()>,
}
impl Running {
    fn finish(self) -> Shutdown {
        let result = self.done.recv_timeout(Duration::from_secs(5)).unwrap();
        self.join.join().unwrap();
        result
    }
}
fn launch(
    server: &Server,
    observer: &X11Pointer,
    seat: &Seat,
    policy: Policy,
) -> (InputClient, Agent, Running) {
    let rt = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let now = host_now(&cx).unwrap();
    let c = credentials();
    // Explicit local fixture grants; this test neither bypasses nor qualifies
    // Tailscale authentication, approval UI, network transport or rendering.
    let mut authority = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    authority.mark_view_ready(now).unwrap();
    authority.grant_lease(c.lease, now).unwrap();
    authority
        .issue_input_ticket(c.lease, c.ticket, now)
        .unwrap();
    let session = InputSession::new(
        authority,
        c,
        observer.bounds(),
        observer.capabilities(),
        now,
    )
    .unwrap();
    let (agent, driver) = start_x11(
        seat,
        cx,
        session,
        Route::new(7, ProtocolLimits::ABSOLUTE),
        &server.display,
    )
    .unwrap();
    let (send, done) = mpsc::sync_channel(1);
    let join = thread::spawn(move || {
        let _ = send.send(rt.block_on(driver));
    });
    let mut client = InputClient::new(
        c,
        7,
        observer.bounds(),
        observer.capabilities(),
        ProtocolLimits::ABSOLUTE,
        policy,
        local(0),
    )
    .unwrap();
    client.confirm_mapping(c.session, c.view, local(0)).unwrap();
    // Synthetic trusted presentation evidence exercises client gating, not an
    // assertion that a real renderer measured or displayed this observation.
    client
        .presented(
            PresentedObservation {
                session: c.session,
                serial: 0,
                view: c.view,
                received_at: local(0),
                source_age_upper_us: 0,
            },
            local(0),
        )
        .unwrap();
    (client, agent, Running { done, join })
}
fn drag(client: &mut InputClient) -> Vec<u8> {
    let mut bytes = vec![0; MAX_INPUT_RECORD_BYTES];
    let encoded = client
        .action(
            Action::Button {
                button: PointerButton::Primary,
                pressed: true,
                position: DesktopPoint { x: 30, y: 40 },
            },
            &mut bytes,
            local(1),
        )
        .unwrap();
    assert_eq!(encoded.sequence, 0);
    bytes.truncate(encoded.bytes);
    bytes
}
fn submitted(agent: &mut Agent, bytes: &[u8]) -> InputResult {
    agent.submit(bytes, InputDelivery::Reliable).unwrap();
    let result = receipt(reply(agent));
    assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(result.submitted_operations, 2);
    result
}
#[test]
fn client_bytes_native_drag_receipt_and_old_pointer_then_stale_view_cleanup() {
    let server = Server::start();
    let mut observer = X11Pointer::open(&server.display).unwrap();
    let seat = Seat::default();
    let (mut client, mut agent, running) = launch(&server, &observer, &seat, Policy::default());
    let mut late = vec![0; MAX_INPUT_RECORD_BYTES];
    let encoded = client
        .pointer(DesktopPoint { x: 1, y: 2 }, &mut late, local(1))
        .unwrap();
    late.truncate(encoded.bytes);
    let bytes = drag(&mut client);
    let result = submitted(&mut agent, &bytes);
    let result_bytes = encode_result(result);
    assert_eq!(
        client.result(&result_bytes, local(2)),
        Ok(ResultEvent::Completed(result))
    );
    assert_eq!(client.pending_actions(), 0);
    eventually(|| observer.query_pointer().unwrap() == (DesktopPoint { x: 30, y: 40 }, 256));
    agent.submit(&late, InputDelivery::Datagram).unwrap();
    assert_eq!(
        reply(&mut agent),
        Reply::Input(Ok(Dispatch::ObsoletePointer))
    );
    assert_eq!(
        observer.query_pointer().unwrap(),
        (DesktopPoint { x: 30, y: 40 }, 256)
    );
    // Even the client's byte-identical accidental duplicate cannot execute a
    // second press. The same terminal receipt is returned by both owners.
    assert_eq!(submitted(&mut agent, &bytes), result);
    assert_eq!(
        client.result(&result_bytes, local(3)),
        Ok(ResultEvent::Duplicate(result))
    );
    assert_eq!(
        client.tick(local(250_000)),
        Err(Error::Stopped(StopReason::ViewStale))
    );
    assert!(
        client
            .pointer(DesktopPoint { x: 90, y: 90 }, &mut late, local(250_000))
            .is_err()
    );
    // Client stop alone is NOT native release. The containing coordinator sends
    // this lifecycle decision to the independent host revoke path explicitly.
    agent.control().stop(HostStop::ViewInvalidated);
    let done = running.finish();
    assert!(done.handoff_safe());
    assert_eq!(done.reason, HostStop::ViewInvalidated);
    assert!(!seat.is_occupied());
    assert_eq!(observer.query_pointer().unwrap().1 & 256, 0);
    assert!(
        client
            .ticket(InputTicketId::from_raw(4), local(250_000))
            .is_err()
    );
}
#[test]
fn timed_out_client_collects_late_real_submission_without_reopening_or_losing_cleanup() {
    let server = Server::start();
    let mut observer = X11Pointer::open(&server.display).unwrap();
    let seat = Seat::default();
    let (mut client, mut agent, running) = launch(
        &server,
        &observer,
        &seat,
        Policy {
            view_age_us: 250_000,
            receipt_timeout_us: 10,
        },
    );
    let bytes = drag(&mut client);
    agent.submit(&bytes, InputDelivery::Reliable).unwrap();
    eventually(|| observer.query_pointer().unwrap().1 & 256 != 0);
    // Deliberately withhold collecting the native reply to model a delayed
    // result. The effect exists despite the client not having its receipt yet.
    assert_eq!(
        client.tick(local(11)),
        Err(Error::Stopped(StopReason::ReceiptTimeout))
    );
    agent.control().stop(HostStop::ClientDisconnected);
    assert!(running.finish().handoff_safe());
    assert_eq!(observer.query_pointer().unwrap().1 & 256, 0);
    let result = receipt(reply(&mut agent));
    assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(
        client.result(&encode_result(result), local(12)),
        Ok(ResultEvent::Completed(result))
    );
    assert_eq!(client.stopped(), Some(StopReason::ReceiptTimeout));
    assert!(!seat.is_occupied());
}
