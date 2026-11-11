//! Successful explicit reopens through the actual public LocalAPI/TLS/UDP
//! supervisor. There is no native desktop here. Session cleanup reports and
//! post-close application failures are explicit fixtures, never real tailnet
//! membership, rendered pixels, or OS input-release qualification.
use super::*;
use crate::{
    native_connection::reconnect::{
        self, Application, CallbackError, CompletionAction, Failure, Status,
    },
    session_startup::{ObserverError, StreamingViewerError, ViewerSession},
    worker::Deadline,
};
use asupersync::types::CancelKind;
use fr_transport::quic::{self, ClosedReport, ConnectionBinding};
use fr_wire::{
    authority::Binding,
    closure::{Cleanup, Closed, ClosedReason, OutstandingEffects, Reason},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Normal,
    CleanupFails,
    CleaningRefuses,
    CancelCompletion,
    ReopeningRefuses,
    ChangeIdentity,
    AdverseReport,
    CompletionRefuses,
    CancelCompletionRefuses,
    FailureBudget,
}
struct App {
    mode: Mode,
    supervisor: Cx,
    changed: Arc<AtomicBool>,
    opened: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
    events: Vec<Status>,
    bindings: Vec<ConnectionBinding>,
    owner: Option<ViewerSession>,
    cleaned: u8,
    runs: u8,
    decisions: u8,
    reopens: u8,
    report: Option<Closed>,
    reopening_at: Option<std::time::Instant>,
    reopen_gaps: Vec<Duration>,
}
impl Application for App {
    type Output = ();
    async fn run(&mut self, attempt: u8, mut viewer: Viewer) -> Result<(), ObserverError> {
        assert_eq!(self.cleaned + 1, attempt);
        assert!(self.owner.is_none());
        self.runs += 1;
        while !viewer.is_complete() {
            viewer
                .drive(Duration::from_millis(1))
                .await
                .map_err(ObserverError::Session)?;
        }
        self.owner = Some(viewer.finish().map_err(ObserverError::Session)?);
        let running = self.owner.as_mut().unwrap();
        let q = running.io().unwrap().0;
        for retired in &self.bindings {
            assert!(
                !q.is_bound_to(retired),
                "equal fixture IDs cannot reuse a connection owner"
            );
        }
        self.bindings.push(q.binding());
        while self.opened.load(Ordering::Acquire) < usize::from(attempt) {
            running
                .drive(Duration::from_millis(1), |_, _| Err(()))
                .await
                .map_err(ObserverError::Session)?;
        }
        for _ in 0..4 {
            running
                .drive(Duration::from_millis(1), |_, _| Err(()))
                .await
                .map_err(ObserverError::Session)?;
        }
        let outcome = running
            .disconnect(Reason::Requested)
            .await
            .map_err(ObserverError::Session)?;
        assert_eq!(outcome.transport, Ok(()));
        assert!(outcome.report.is_some());
        self.report = outcome.report;
        // This tests retry accounting after actual successful admission and
        // closure; the application failure itself is deliberately injected.
        if self.mode == Mode::FailureBudget && matches!(attempt, 2 | 4) {
            Err(ObserverError::Streaming(StreamingViewerError::Transport(
                quic::Error::Expired,
            )))
        } else {
            Ok(())
        }
    }
    async fn cleanup(&mut self, cx: &Cx, deadline: Deadline) -> Result<(), CallbackError> {
        assert!(
            cx.checkpoint().is_ok(),
            "cleanup has an independent context"
        );
        if let Some(mut owner) = self.owner.take() {
            assert!(owner.is_closed());
            owner.close();
        }
        self.cleaned += 1;
        if self.mode == Mode::CleanupFails {
            return Err(CallbackError);
        }
        // A failed fresh identity check never opened another server session.
        if self.runs == self.cleaned {
            while self.closed.load(Ordering::Acquire) < usize::from(self.cleaned) {
                cx.checkpoint().map_err(|_| CallbackError)?;
                if cx.now() >= deadline.time() {
                    return Err(CallbackError);
                }
                sleep(cx.now(), Duration::from_millis(1)).await;
            }
        }
        Ok(())
    }
    fn completion_action(&mut self) -> Result<CompletionAction, CallbackError> {
        assert_eq!(self.cleaned, self.runs, "choice before original cleanup");
        assert!(self.owner.is_none());
        self.decisions += 1;
        if matches!(
            self.mode,
            Mode::CancelCompletion | Mode::CancelCompletionRefuses
        ) {
            self.supervisor.cancel_fast(CancelKind::User);
        }
        if matches!(
            self.mode,
            Mode::CompletionRefuses | Mode::CancelCompletionRefuses
        ) {
            return Err(CallbackError);
        }
        Ok(if self.decisions <= self.reopens {
            CompletionAction::Reopen
        } else {
            CompletionAction::Finish
        })
    }
    fn terminal_failure(&self) -> Option<ObserverError> {
        assert!(self.cleaned > 0);
        if matches!(
            self.mode,
            Mode::CompletionRefuses | Mode::CancelCompletionRefuses
        ) {
            // Explicit application-owned protocol-error fixture after an actual
            // admitted connection/close. This checks exact error priority; it
            // does not claim a malformed packet was supplied by the real peer.
            return Some(protocol_failure());
        }
        self.report
            .filter(|report| report.reason != ClosedReason::ClientRequested)
            .map(|report| {
                ObserverError::Streaming(StreamingViewerError::Session(
                    crate::session_startup::Error::RemoteClosed(report),
                ))
            })
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        if let Status::Connecting { attempt } = status {
            assert_eq!(self.cleaned + 1, attempt, "new owner before cleanup");
            self.report = None;
            if let Some(at) = self.reopening_at.take() {
                self.reopen_gaps.push(at.elapsed());
            }
        }
        if let Status::Reopening { .. } = status {
            self.reopening_at = Some(std::time::Instant::now());
        }
        self.events.push(status);
        match (self.mode, status) {
            (Mode::CleaningRefuses, Status::Cleaning { .. })
            | (Mode::ReopeningRefuses, Status::Reopening { .. }) => Err(CallbackError),
            (Mode::ChangeIdentity, Status::Reopening { .. }) => {
                self.changed.store(true, Ordering::Release);
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

fn protocol_failure() -> ObserverError {
    ObserverError::Streaming(StreamingViewerError::Transport(quic::Error::Malformed))
}

fn report(mode: Mode) -> Closed {
    Closed {
        reason: if mode == Mode::AdverseReport {
            ClosedReason::PermissionLost
        } else {
            ClosedReason::ClientRequested
        },
        cleanup: Cleanup::Complete,
        // Explicit accounting fixture, retained even on an adverse report.
        effects: OutstandingEffects::Known {
            pending: 2,
            uncertain: 3,
        },
    }
}

async fn host_attempt(
    cx: &Cx,
    cleanup: &Cx,
    endpoint: QuicUdpEndpoint,
    opened: &AtomicUsize,
    attempt: usize,
    final_report: Closed,
) {
    let native = accept(cx, endpoint).await;
    let mut host = crate::session_startup::connection_test_host(
        cx.clone(),
        native,
        offer(),
        Policy::default(),
    );
    let mut approved = false;
    while !host.is_complete() {
        host.drive(Duration::from_millis(1)).await.unwrap();
        if !approved && let Some(approval) = host.approval() {
            approval.decide(true).unwrap();
            approved = true;
        }
    }
    assert!(approved);
    let mut running = host.finish().unwrap().into_running().unwrap();
    let control = running.observation().unwrap();
    let binding = running.binding();
    let (q, routes) = running.io().unwrap();
    let original = q.binding();
    let owner = ClosedReport::default();
    q.arm_closed_report(
        cleanup,
        &original,
        routes.outbound,
        Binding {
            channel: binding.id,
            session: binding.remote_session,
        },
        owner.registration(),
        move || control.revoke(),
    )
    .unwrap();
    opened.store(attempt, Ordering::Release);
    let mut nonce = 2_000;
    while running
        .drive(
            Duration::from_millis(1),
            || {
                nonce += 1;
                Ok(nonce)
            },
            |_, _| panic!("only the original session close is expected"),
        )
        .await
        .is_ok()
    {}
    assert_eq!(owner.finish(final_report).await, Some(Ok(())));
}

#[allow(clippy::too_many_lines)]
async fn scenario(
    runtime: &asupersync::runtime::Runtime,
    mode: Mode,
    reopens: u8,
    automatic_attempts: u8,
) -> (Result<(), Failure>, App) {
    let api = Api::new();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let first_host = runtime.request_cx_with_budget(Budget::INFINITE);
    let timeout_cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let endpoint = endpoint(&first_host).await;
    let address = endpoint.local_addr();
    let mut client = Client::new(api.client.clone(), roots(), Duration::from_secs(4)).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let mut app = App {
        mode,
        supervisor: cx.clone(),
        changed: api.changed.clone(),
        opened: Arc::new(AtomicUsize::new(0)),
        closed: Arc::new(AtomicUsize::new(0)),
        events: vec![],
        bindings: vec![],
        owner: None,
        cleaned: 0,
        runs: 0,
        decisions: 0,
        reopens,
        report: None,
        reopening_at: None,
        reopen_gaps: vec![],
    };
    let (opened, closed) = (app.opened.clone(), app.closed.clone());
    let client_done = done.clone();
    let client_run = async {
        let result = client
            .run_observing(
                cx,
                runtime.handle(),
                PeerSelector::StableId("n-peer"),
                Configuration {
                    port: address.port(),
                    ..Configuration::default()
                },
                offer(),
                reconnect::Policy {
                    max_attempts: automatic_attempts,
                    initial_backoff: Duration::from_millis(10),
                    maximum_backoff: Duration::from_millis(20),
                    cleanup_timeout: Duration::from_secs(2),
                },
                &mut app,
            )
            .await;
        client_done.store(true, Ordering::Release);
        result
    };
    let server = async {
        let mut next = Some(endpoint);
        for attempt in 1..=32 {
            let host_cx = runtime.request_cx_with_budget(Budget::INFINITE);
            let cleanup = runtime.request_cx_with_budget(Budget::INFINITE);
            let endpoint = match next.take() {
                Some(endpoint) => endpoint,
                None => QuicUdpEndpoint::bind(
                    &host_cx,
                    address,
                    QuicUdpEndpointConfig {
                        max_packet_size: 1200,
                        max_batch_size: 16,
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
            };
            Box::pin(host_attempt(
                &host_cx,
                &cleanup,
                endpoint,
                &opened,
                attempt,
                report(mode),
            ))
            .await;
            closed.store(attempt, Ordering::Release);
        }
    };
    let mut server = pin!(server);
    let serving = poll_fn(|task| {
        if done.load(Ordering::Acquire) {
            Poll::Ready(())
        } else {
            server.as_mut().poll(task)
        }
    });
    let (result, ()) = Box::pin(timeout(
        timeout_cx.now(),
        Duration::from_secs(120),
        network::both(client_run, serving),
    ))
    .await
    .unwrap();
    assert_eq!(
        app.cleaned as usize,
        app.events
            .iter()
            .filter(|s| matches!(s, Status::Connecting { .. }))
            .count()
    );
    assert!(api.calls.load(Ordering::Acquire) >= usize::from(app.runs));
    (result, app)
}

#[test]
#[ignore = "explicit isolated namespace: scripts/verify-native-session.sh"]
fn explicit_reopens_work_with_one_automatic_attempt_and_fresh_connection_owners() {
    let runtime = network::runtime();
    let (result, app) = runtime.block_on(Box::pin(scenario(&runtime, Mode::Normal, 2, 1)));
    assert_eq!(result, Ok(()));
    assert_eq!((app.runs, app.cleaned, app.decisions), (3, 3, 3));
    assert_eq!(app.bindings.len(), 3);
    assert_eq!(
        app.events
            .iter()
            .filter(|s| matches!(s, Status::Reopening { .. }))
            .count(),
        2
    );
    assert!(
        !app.events
            .iter()
            .any(|s| matches!(s, Status::Reconnecting { .. }))
    );
    assert_eq!(app.events.last(), Some(&Status::Completed { attempt: 3 }));
    // Each reopened attempt waits the first retry delay (10 ms here) before
    // it dials, so the host can bind its next listener (fr-w4yu).
    assert_eq!(app.reopen_gaps.len(), 2);
    assert!(
        app.reopen_gaps
            .iter()
            .all(|gap| *gap >= Duration::from_millis(10)),
        "{:?}",
        app.reopen_gaps
    );
}

#[test]
#[ignore = "explicit isolated namespace: scripts/verify-native-session.sh"]
fn successful_choices_do_not_replenish_the_original_automatic_retry_budget() {
    let runtime = network::runtime();
    let (result, app) = runtime.block_on(Box::pin(scenario(&runtime, Mode::FailureBudget, 3, 2)));
    let failure = Failure::Observation(ObserverError::Streaming(StreamingViewerError::Transport(
        quic::Error::Expired,
    )));
    assert_eq!(result, Err(failure));
    assert_eq!((app.runs, app.cleaned, app.decisions), (4, 4, 2));
    assert_eq!(
        app.events
            .iter()
            .filter(|s| matches!(s, Status::Reconnecting { .. }))
            .count(),
        1
    );
    assert_eq!(
        app.events.last(),
        Some(&Status::Stopped {
            attempt: 4,
            failure
        })
    );
}

#[test]
#[ignore = "explicit isolated namespace: scripts/verify-native-session.sh"]
fn cleanup_refusal_cancellation_and_adverse_reports_prevent_explicit_reopening() {
    let runtime = network::runtime();
    for (mode, expected, decisions) in [
        (Mode::CleanupFails, Failure::Cleanup, 0),
        (Mode::CleaningRefuses, Failure::Notification, 0),
        (Mode::CancelCompletion, Failure::Cancelled, 1),
        (Mode::ReopeningRefuses, Failure::Notification, 1),
        (
            Mode::CompletionRefuses,
            Failure::Observation(protocol_failure()),
            1,
        ),
        (Mode::CancelCompletionRefuses, Failure::Cancelled, 1),
        (
            Mode::AdverseReport,
            Failure::Observation(ObserverError::Streaming(StreamingViewerError::Session(
                crate::session_startup::Error::RemoteClosed(report(Mode::AdverseReport)),
            ))),
            1,
        ),
    ] {
        let (result, app) = runtime.block_on(Box::pin(scenario(&runtime, mode, 1, 1)));
        assert_eq!(result, Err(expected), "{mode:?}");
        assert_eq!(
            (app.runs, app.cleaned, app.decisions),
            (1, 1, decisions),
            "{mode:?}"
        );
        assert_eq!(
            app.report,
            Some(report(mode)),
            "original effects remain inspectable"
        );
    }
}

#[test]
#[ignore = "explicit isolated namespace: scripts/verify-native-session.sh"]
fn explicit_reopen_revalidates_the_original_pinned_identity() {
    let runtime = network::runtime();
    let (result, app) = runtime.block_on(Box::pin(scenario(&runtime, Mode::ChangeIdentity, 1, 1)));
    assert_eq!(
        result,
        Err(Failure::Connection(Error::Tailnet(
            fr_tailnet::Error::IdentityChanged
        )))
    );
    assert_eq!((app.runs, app.cleaned, app.decisions), (1, 2, 1));
}

#[test]
#[ignore = "explicit isolated namespace: scripts/verify-native-session.sh"]
fn explicit_choices_cannot_wrap_or_exceed_the_total_native_owner_limit() {
    let runtime = network::runtime();
    let (result, app) = runtime.block_on(Box::pin(scenario(&runtime, Mode::Normal, 32, 1)));
    assert_eq!(result, Err(Failure::AttemptLimit));
    assert_eq!((app.runs, app.cleaned, app.decisions), (32, 32, 32));
    assert_eq!(
        app.events.last(),
        Some(&Status::Stopped {
            attempt: 32,
            failure: Failure::AttemptLimit
        })
    );
}
