//! Serial retirement with actual UDP/TLS, authenticated local HTTP and ingress
//! lease custody. Interface/firewall replies are the existing explicit fixtures.
//! Run ignored cases through scripts/test_linux_serial_lifecycle.sh with the frd
//! library test binary and host_run::retirement_tests as its filter. No physical
//! GPU, installed-tailnet or whole-process host-run qualification is claimed.
use super::*;
use crate as frd;
use crate::{
    native_connection::host::{Error as HostError, IngressCheck},
    session_startup::{Host, HostSession, Viewer, ViewerSession},
};
use asupersync::{runtime::RuntimeHandle, time::timeout};
use fr_transport::native_accept::Listener;
use std::{fs, net::UdpSocket, os::unix::fs::PermissionsExt, sync::atomic::AtomicU64};

#[allow(dead_code)]
#[path = "../../tests/native_host_accept/fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "../../../fr-transport/tests/support/mod.rs"]
mod network;

struct Tools(PathBuf);
impl Tools {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        assert_eq!(
            fs::read_to_string("/run/fr-synthetic-ingress").unwrap(),
            "synthetic-only\n"
        );
        let path = PathBuf::from(format!(
            "/run/fr-retirement-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let script = include_str!("../../tests/native_host_linux_serial/tools.py").replace(
            "script = sys.stdin.read(4096)",
            "script = sys.stdin.read(4096)\n    deny = root / 'delete-policy'\n    if script.startswith('delete table') and deny.exists() and deny.read_text() == 'deny': raise SystemExit(1)",
        );
        for name in ["nft", "ip"] {
            crate::executable::write(&path.join(name), &script);
        }
        Self(path)
    }
    fn state(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.0.join("state.json")).unwrap()).unwrap()
    }
}

struct LocalHost {
    options: Options,
    report: Reporter,
    stop: Arc<StopHandle>,
    active: Arc<Mutex<Option<Cx>>>,
    seat: Seat,
}
impl LocalHost {
    fn new(api: &fixture::Api, tools: &Tools) -> Self {
        Self {
            options: Options {
                socket: Some(api.path.clone()),
                port: address().port(),
                interface: "fr-fixture".into(),
                // These tests never start a capture worker or native input.
                worker: PathBuf::from("/fixture-not-launched"),
                display: ":0".into(),
                xauthority: None,
                trust_roots: fixture::pki().join("ca.pem"),
                sharing: Scope::OwnUser,
                fps: 30,
                bitrate: 2_000_000,
                adaptive_capture: false,
                ingress_tools: Some((tools.0.join("nft"), tools.0.join("ip"))),
                once: false,
                handle_signals: false,
                input_agent: None,
                clipboard: false,
                audio: None,
                microphone: None,
                files: None,
                file_send: None,
                directory: false,
                session_monitor: None,
            },
            report: Arc::new(|_| {}),
            stop: Arc::new(StopHandle::default()),
            active: Arc::new(Mutex::new(None)),
            seat: Seat::default(),
        }
    }
    fn share<'a>(
        &'a self,
        runtime: &'a RuntimeHandle,
        api: &'a LocalApi,
        identity: &'a fr_tailnet::NativeServerIdentity,
    ) -> Share<'a> {
        Share {
            options: &self.options,
            runtime,
            api,
            identity,
            policy: None,
            host_boot: HostBootId::from_raw(700),
            stop: self.stop.clone(),
            active: self.active.clone(),
            report: &self.report,
            seat: self.seat.clone(),
            lifetime: None,
            control: None,
            encoder: Encoder::SoftwareExplicit,
            approval: None,
        }
    }
}

fn address() -> SocketAddr {
    "100.64.0.1:4792".parse().unwrap()
}

fn run(body: impl AsyncFnOnce(Cx, RuntimeHandle)) {
    let runtime = network::runtime();
    let broker = runtime.request_cx_with_budget(Budget::INFINITE);
    let handle = runtime.handle();
    runtime.block_on(async {
        timeout(
            broker.now(),
            Duration::from_secs(20),
            body(broker.clone(), handle),
        )
        .await
        .unwrap();
    });
}

fn request_for(share: &Share<'_>) -> Request {
    let mut request = request(
        1,
        share.host_boot,
        random_nonzero_u32().unwrap(),
        Scope::OwnUser,
        (None, false, false, false, false),
        false,
        false,
    )
    .unwrap();
    // Exercise the actual session/observation owners without claiming media
    // readiness. Fresh production-allocated IDs and policy are unchanged.
    request.session.offer = fixture::offer();
    request
}

struct Peer {
    request: Option<Request>,
    retained: Option<HostSession>,
    observation: Option<ObservationControl>,
    receipt: Option<Result<u128, HostError>>,
    retain: bool,
}
impl serial::Application for Peer {
    type Output = u128;
    fn request(&mut self, attempt: u64) -> Result<Request, serial::Error> {
        assert_eq!(attempt, 1, "no replacement before original retirement");
        self.request.take().ok_or(serial::Error::IdentityReuse)
    }
    async fn serve(&mut self, host: Host) -> u128 {
        let mut session = host
            .open(Duration::from_millis(5), |approval, _| {
                approval.decide(true).map_err(|_| ())
            })
            .await
            .unwrap();
        let id = session.binding().remote_session.as_raw();
        self.observation = Some(session.observation().unwrap());
        if self.retain {
            self.retained = Some(session);
        }
        id
    }
    fn completed(
        &mut self,
        statistics: serial::Statistics,
        outcome: Result<u128, HostError>,
    ) -> Result<serial::Action, serial::Error> {
        assert_eq!(statistics.attempts, 1);
        assert_eq!(statistics.admitted, 1);
        assert_eq!(statistics.refused, 0);
        assert!(self.receipt.replace(outcome).is_none());
        Ok(serial::Action::Stop)
    }
}

async fn exchange(
    server: &mut LinuxServer,
    runtime: &RuntimeHandle,
    request: Request,
    retain: bool,
) -> (Result<serial::Statistics, LinuxError>, Peer, ViewerSession) {
    let supervisor = runtime
        .try_request_cx_with_budget(Budget::INFINITE)
        .unwrap();
    let client = runtime
        .try_request_cx_with_budget(Budget::INFINITE)
        .unwrap();
    let mut peer = Peer {
        request: Some(request),
        retained: None,
        observation: None,
        receipt: None,
        retain,
    };
    let viewer = async {
        let native = fixture::client(&client, address()).await;
        let mut viewer = Viewer::new(
            client,
            native,
            fixture::offer(),
            fr_transport::quic::Policy::default(),
            Duration::from_secs(3),
        )
        .unwrap();
        while !viewer.is_complete() {
            viewer.drive(Duration::from_millis(5)).await.unwrap();
        }
        viewer.finish().unwrap()
    };
    let (result, viewer) = Box::pin(network::both(
        server.serve_serial(
            supervisor.clone(),
            runtime.clone(),
            serial::Policy {
                retirement_timeout: Duration::from_millis(80),
                ..serial::Policy::default()
            },
            &mut peer,
        ),
        viewer,
    ))
    .await;
    assert!(supervisor.is_cancel_requested());
    assert!(peer.observation.as_ref().unwrap().check().is_err());
    (result, peer, viewer)
}

async fn cleanup(share: &Share<'_>, server: &mut LinuxServer) -> Result<(), Error> {
    let source = share.cx().unwrap();
    let audio = Arc::new(Mutex::new(None));
    let mut driver = share.driver(&source, 101, &audio).unwrap();
    // No source was started; the actual cleanup must still refuse while the
    // escaped peer keeps the original ingress lease, despite cancellation.
    share
        .cleanup(&mut driver, &Mutex::new(None), &audio, server)
        .await
}

fn retained_error() -> LinuxError {
    LinuxError::Serial(serial::Error::RetainedTransport)
}

fn retryable(result: Result<Ended, Error>) -> Error {
    match result {
        Ok(Ended::Failed(error)) => error,
        Err(error) => panic!("cleanup-confirmed share must be retryable: {error:?}"),
        Ok(_) => panic!("a retirement timeout must retain its failure outcome"),
    }
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn delayed_original_transport_recovers_after_cleanup_into_a_fresh_session() {
    run(async |broker, runtime| {
        let tools = Tools::new();
        let api = fixture::Api::new();
        let identity = api.identity(&broker).await;
        let host = LocalHost::new(&api, &tools);
        let share = host.share(&runtime, &api.client, &identity);
        let mut server = share.bind(&broker).await.unwrap();
        let first = request_for(&share);
        let old_binding = first.session.binding;
        let old_connection = first.connection_id;
        let (end, mut old, viewer) = exchange(&mut server, &runtime, first, true).await;
        assert_eq!(end, Err(retained_error()));
        assert_eq!(old.receipt, Some(Ok(old_binding.remote_session.as_raw())));
        assert!(UdpSocket::bind(address()).is_err());
        assert_eq!(tools.state()["deleted"], 0);
        // Deliberately held beyond the serial bound; release only this original
        // owner, then require the production cleanup to prove actual retirement.
        drop(old.retained.take());
        drop(viewer);
        let cleaned = cleanup(&share, &mut server).await;
        let failure = retryable(share.after_cleanup(End::Listener(end), cleaned, None));
        assert_eq!(failure, Error::Listener(Box::new(retained_error())));
        assert_eq!(tools.state()["deleted"], 1);
        assert!(UdpSocket::bind(address()).is_ok());
        let mut failures = 0;
        record_share_failure(false, &mut failures, failure).unwrap();
        assert_eq!(failures, 1);
        backoff(&broker, &host.stop, failures).await;

        let fresh_share = host.share(&runtime, &api.client, &identity);
        let mut fresh_server = fresh_share.bind(&broker).await.unwrap();
        let fresh = request_for(&fresh_share);
        assert_eq!(fresh.session.binding.host_boot, old_binding.host_boot);
        assert_ne!(fresh.session.binding.os_session, old_binding.os_session);
        assert_ne!(
            fresh.session.binding.remote_session,
            old_binding.remote_session
        );
        assert_ne!(fresh.connection_id, old_connection);
        let (end, new, viewer) = exchange(&mut fresh_server, &runtime, fresh, false).await;
        assert!(end.is_ok(), "{end:?}");
        assert!(new.receipt.as_ref().unwrap().is_ok());
        assert!(old.observation.as_ref().unwrap().check().is_err());
        drop(viewer);
        let cleaned = cleanup(&fresh_share, &mut fresh_server).await;
        assert!(matches!(
            fresh_share.after_cleanup(End::Listener(end), cleaned, None),
            Ok(Ended::Served)
        ));
        assert_eq!(tools.state()["created"], 2);
        assert_eq!(tools.state()["deleted"], 2);
        assert!(UdpSocket::bind(address()).is_ok());
    });
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn still_retained_transport_prevents_cleanup_and_any_restart() {
    run(async |broker, runtime| {
        let tools = Tools::new();
        let api = fixture::Api::new();
        let identity = api.identity(&broker).await;
        let host = LocalHost::new(&api, &tools);
        let share = host.share(&runtime, &api.client, &identity);
        let mut server = share.bind(&broker).await.unwrap();
        let (end, mut peer, viewer) =
            exchange(&mut server, &runtime, request_for(&share), true).await;
        assert_eq!(end, Err(retained_error()));
        let cleaned = cleanup(&share, &mut server).await;
        assert_eq!(cleaned, Err(Error::Cleanup("ingress")));
        assert_eq!(
            share.after_cleanup(End::Listener(end), cleaned, None).err(),
            Some(Error::Cleanup("ingress"))
        );
        assert_eq!(tools.state()["created"], 1);
        assert_eq!(tools.state()["deleted"], 0);
        assert!(UdpSocket::bind(address()).is_err());
        drop(peer.retained.take());
        drop(viewer);
        server.stop(&broker).await.unwrap();
        assert_eq!(tools.state()["deleted"], 1);
    });
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn failed_rule_cleanup_cannot_turn_a_retirement_timeout_into_a_restart() {
    run(async |broker, runtime| {
        let tools = Tools::new();
        let api = fixture::Api::new();
        let identity = api.identity(&broker).await;
        let host = LocalHost::new(&api, &tools);
        let share = host.share(&runtime, &api.client, &identity);
        let mut server = share.bind(&broker).await.unwrap();
        let (end, mut peer, viewer) =
            exchange(&mut server, &runtime, request_for(&share), true).await;
        assert_eq!(end, Err(retained_error()));
        drop(peer.retained.take());
        drop(viewer);
        fs::write(tools.0.join("delete-policy"), "deny").unwrap();
        let cleaned = cleanup(&share, &mut server).await;
        assert_eq!(cleaned, Err(Error::Cleanup("ingress")));
        assert_eq!(
            share.after_cleanup(End::Listener(end), cleaned, None).err(),
            Some(Error::Cleanup("ingress"))
        );
        assert_eq!(tools.state()["created"], 1);
        assert_eq!(tools.state()["deleted"], 0);
        // Clean up only the existing fixture owner; no replacement is attempted.
        fs::write(tools.0.join("delete-policy"), "allow").unwrap();
        server.stop(&broker).await.unwrap();
    });
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn cleanup_failures_and_other_listener_faults_remain_terminal() {
    run(async |broker, runtime| {
        let tools = Tools::new();
        let api = fixture::Api::new();
        let identity = api.identity(&broker).await;
        let host = LocalHost::new(&api, &tools);
        let share = host.share(&runtime, &api.client, &identity);
        // These are classification negatives; no successful cleanup receipt or
        // restarted listener is fabricated by them.
        for stage in ["source", "launch", "audio", "ingress", "approval"] {
            assert_eq!(
                share
                    .after_cleanup(
                        End::Listener(Err(retained_error())),
                        Err(Error::Cleanup(stage)),
                        None,
                    )
                    .err(),
                Some(Error::Cleanup(stage))
            );
        }
        for failure in [
            LinuxError::Ingress(ingress::Error::FirewallMismatch),
            LinuxError::Host(HostError::IngressUnavailable),
            LinuxError::Serial(serial::Error::Clock),
            LinuxError::Serial(serial::Error::IdentityReuse),
            LinuxError::Serial(serial::Error::Callback),
            LinuxError::Spent,
        ] {
            assert_eq!(
                share
                    .after_cleanup(End::Listener(Err(failure)), Ok(()), None)
                    .err(),
                Some(Error::Listener(Box::new(failure)))
            );
        }
    });
}

#[test]
fn one_shot_retirement_failure_keeps_its_typed_outcome() {
    let mut failures = 0;
    let failure = Error::Listener(Box::new(retained_error()));
    assert_eq!(
        record_share_failure(true, &mut failures, failure.clone()),
        Err(failure)
    );
    assert_eq!(failures, 0);
}

#[test]
fn source_and_retirement_failures_share_one_bounded_restart_budget() {
    let mut failures = 0;
    for attempt in 1..=MAX_CONSECUTIVE_FAILURES {
        let failure = if attempt.is_multiple_of(2) {
            Error::Desktop(dispatch::Error::Clock)
        } else {
            Error::Listener(Box::new(retained_error()))
        };
        let result = record_share_failure(false, &mut failures, failure.clone());
        assert_eq!(failures, attempt);
        if attempt == MAX_CONSECUTIVE_FAILURES {
            assert_eq!(result, Err(failure));
        } else {
            assert_eq!(result, Ok(()));
        }
    }
}
