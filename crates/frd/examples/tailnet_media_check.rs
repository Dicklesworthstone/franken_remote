//! Explicit root-owned `LocalAPI` fixture through actual media admission.
//! No Tailscale device, GUI capture, public listener or synthetic proof constructor.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("tailnet_media_check requires Linux");
    std::process::exit(2);
}
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}
#[cfg(target_os = "linux")]
mod linux {
    use asupersync::{
        cx::Cx, net::unix::UnixStream, runtime::RuntimeBuilder, time::sleep, types::Budget,
    };
    use fr_core::{
        authority::{AuthorityPolicy, SessionAuthority},
        ids::{CodecConfigurationGeneration, RecoveryGeneration, RemoteSessionId},
        limits::ProtocolLimits,
    };
    use fr_media::{
        access_unit::{EncodedAccessUnit, FrameId, FrameKind},
        delivery::{MediaBindings, MediaEpoch, SendPolicy},
    };
    use fr_tailnet::{Admission, ConnectionAddresses, GrantPolicy, LocalApi};
    use fr_wire::MediaLimits;
    use frd::{
        media::{ObservationControl, Subscription, host_now},
        media_egress::{Admission as Delivery, Egress, EgressError, Lane},
    };
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    struct Fixture {
        api: LocalApi,
        stop: Arc<AtomicBool>,
        denied: Arc<AtomicBool>,
        input_allowed: Arc<AtomicBool>,
        task: Option<JoinHandle<()>>,
    }
    impl Fixture {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path: PathBuf = std::env::temp_dir().join(format!(
                "fr-admission-media-{}-{stamp}.sock",
                std::process::id()
            ));
            let listener = UnixListener::bind(&path).unwrap();
            listener.set_nonblocking(true).unwrap();
            let api = LocalApi::new(path).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let denied = Arc::new(AtomicBool::new(false));
            let input_allowed = Arc::new(AtomicBool::new(false));
            let input_permission = input_allowed.clone();
            let (end, changed) = (stop.clone(), denied.clone());
            let task = thread::spawn(move || {
                let host_key = format!("nodekey:{}", "1".repeat(64));
                let key = format!("nodekey:{}", "2".repeat(64));
                let status = format!(
                    r#"{{"Version":"synthetic-root-fixture","BackendState":"Running","TailscaleIPs":["100.64.0.1"],"CurrentTailnet":{{"Name":"test.invalid","MagicDNSSuffix":"test.ts.net"}},"Self":{{"ID":"host","NodeID":1,"PublicKey":"{host_key}","UserID":7,"TailscaleIPs":["100.64.0.1"],"InNetworkMap":true}},"Peer":{{"{key}":{{"ID":"peer","NodeID":2,"PublicKey":"{key}","UserID":7,"TailscaleIPs":["100.64.0.2"],"InNetworkMap":true}}}}}}"#
                );
                while !end.load(Ordering::Acquire) {
                    let (mut socket, _) = match listener.accept() {
                        Ok(v) => v,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(e) => panic!("fixture accept failed: {e}"),
                    };
                    socket
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    socket
                        .set_write_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut byte = [0];
                    while request.len() < 2048 && !request.ends_with(b"\r\n\r\n") {
                        match socket.read(&mut byte) {
                            Ok(1) => request.push(byte[0]),
                            _ => break,
                        }
                    }
                    if request.is_empty() {
                        continue;
                    }
                    let request = String::from_utf8(request).unwrap();
                    assert!(request.contains("Host: local-tailscaled.sock\r\n"));
                    let body = if request.starts_with("GET /localapi/v0/status?peers=true ") {
                        status.clone()
                    } else {
                        assert!(
                            request.starts_with("GET /localapi/v0/whois?addr=100.64.0.2%3A30001 ")
                        );
                        let grant = if changed.load(Ordering::Acquire) {
                            "{}".to_owned()
                        } else {
                            format!(
                                r#"{{"{}":[{{"version":1,"observe":true,"control":{}}}]}}"#,
                                fr_tailnet::DESKTOP_CAPABILITY,
                                input_permission.load(Ordering::Acquire)
                            )
                        };
                        format!(
                            r#"{{"Node":{{"ID":2,"StableID":"peer","Key":"{key}","User":7,"Addresses":["100.64.0.2/32"],"MachineAuthorized":true}},"CapMap":{grant}}}"#
                        )
                    };
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    if let Err(e) = socket.write_all(reply.as_bytes()) {
                        assert!(matches!(
                            e.kind(),
                            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                        ));
                    }
                }
            });
            Self {
                api,
                stop,
                denied,
                input_allowed,
                task: Some(task),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.task.take().unwrap().join().unwrap();
        }
    }
    fn app_authority(cx: &Cx, approved: bool) -> SessionAuthority {
        let mut a = SessionAuthority::new(
            RemoteSessionId::from_raw(9),
            AuthorityPolicy::plan_defaults(),
        );
        a.mark_capabilities_checked().unwrap();
        if approved {
            a.authorize_observation(host_now(cx).unwrap()).unwrap();
        } else {
            a.require_approval().unwrap();
        }
        a
    }
    fn sender(cx: &Cx, control: ObservationControl) -> Egress {
        let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1024, 16384, 64).unwrap();
        let mut e = Egress::new(
            Subscription::new(
                control,
                limits,
                MediaBindings::new(1, 2, 3, 4).unwrap(),
                MediaEpoch {
                    configuration: CodecConfigurationGeneration::INITIAL,
                    recovery: RecoveryGeneration::INITIAL,
                },
                SendPolicy::default(),
            )
            .unwrap(),
        );
        // Opaque packetization bytes, deliberately NOT a codec fixture.
        e.enqueue(
            EncodedAccessUnit::new(
                &ProtocolLimits::ABSOLUTE,
                FrameId::FIRST,
                FrameKind::Idr {
                    recovery: RecoveryGeneration::INITIAL,
                },
                CodecConfigurationGeneration::INITIAL,
                host_now(cx).unwrap().as_micros(),
                vec![7; 5000],
            )
            .unwrap(),
        )
        .unwrap();
        e
    }
    async fn scenario(fixture: &Fixture, cx: Cx, addresses: ConnectionAddresses, case: u8) {
        let policy = GrantPolicy {
            validity: Duration::from_millis(500),
            ..GrantPolicy::default()
        };
        let proof = fixture
            .api
            .authorize_app_capability(&cx, addresses, policy)
            .await
            .unwrap();
        let mut owner = Admission::new(fixture.api.clone(), cx.clone(), proof).unwrap();
        let lease = owner.lease();
        assert_eq!(lease.control(), Err(fr_tailnet::Error::CapabilityDenied));
        // An observation capability is NOT consent: waiting approval stays closed.
        assert!(
            ObservationControl::new_admitted(cx.clone(), app_authority(&cx, false), lease.clone())
                .is_err()
        );
        assert!(lease.observe().is_ok());
        let control =
            ObservationControl::new_admitted(cx.clone(), app_authority(&cx, true), lease.clone())
                .unwrap();
        assert!(
            control
                .deadline(Duration::from_secs(2))
                .unwrap()
                .time()
                .as_nanos()
                / 1000
                <= lease.observe().unwrap()
        );
        let challenge = 123;
        control.issue_challenge(challenge).unwrap();
        let mut egress = sender(&cx, control.clone());
        let mut first = None;
        egress
            .transmit(Lane::Original, |offer, bytes, guard| {
                guard().unwrap();
                first = Some((offer.clone(), bytes.to_vec()));
                Ok::<_, ()>(Delivery::Backpressure)
            })
            .unwrap();
        if case == 4 {
            // Revoke after native preparation, precisely at the existing final guard.
            let result = egress.transmit(Lane::Original, |offer, bytes, guard| {
                let (old, payload) = first.as_ref().unwrap();
                assert_eq!(offer, old);
                assert_eq!(bytes, payload);
                owner.revoke();
                assert!(guard().is_err());
                Err::<Delivery, ()>(())
            });
            assert!(matches!(result, Err(EgressError::Transport(()))));
        } else {
            match case {
                0 => drop(owner),
                1 => {
                    fixture.denied.store(true, Ordering::Release);
                    assert_eq!(
                        owner.refresh().await,
                        Err(fr_tailnet::Error::CapabilityDenied)
                    );
                }
                2 => {
                    sleep(cx.timer_driver().unwrap().now(), Duration::from_millis(550)).await;
                }
                _ => control.suspend(),
            }
            assert!(matches!(
                egress.transmit::<()>(Lane::Original, |_, _, _| panic!(
                    "unauthorized packet reached transport"
                )),
                Err(EgressError::Media(_))
            ));
        }
        assert!(egress.is_closed());
        assert_eq!(egress.allocated_bytes(), 0);
        assert!(lease.observe().is_err());
        assert!(control.renew(challenge).is_err());
        assert!(control.deadline(Duration::from_secs(2)).is_err());
    }

    fn input_credentials() -> fr_core::input::InputCredentials {
        use fr_core::{
            ids::{
                DisplayGeometryGeneration, InputLeaseId, InputTicketId, ViewportMappingGeneration,
            },
            input::{InputCredentials, InputView},
        };
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
    fn input_session(cx: &Cx) -> fr_core::input_submission::InputSession {
        use fr_core::{
            input::{DesktopPoint, InputBounds},
            input_submission::{Capabilities, Capability, InputSession},
        };
        let now = host_now(cx).unwrap();
        let c = input_credentials();
        let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
        a.mark_capabilities_checked().unwrap();
        a.authorize_observation(now).unwrap();
        a.mark_view_ready(now).unwrap();
        a.grant_lease(c.lease, now).unwrap();
        a.issue_input_ticket(c.lease, c.ticket, now).unwrap();
        InputSession::new(
            a,
            c,
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default().with(Capability::Keys),
            now,
        )
        .unwrap()
    }
    struct RecordingInput {
        effects: Arc<std::sync::Mutex<Vec<fr_core::input_submission::Operation>>>,
        gate: fr_tailnet::Lease,
        revoke_in_prepare: bool,
    }
    impl fr_core::input_submission::InputSink for RecordingInput {
        fn prepare(
            &mut self,
            _: fr_core::input_submission::Operation,
        ) -> Result<(), fr_core::input_submission::PlatformError> {
            if self.revoke_in_prepare {
                self.gate.revoke();
            }
            Ok(())
        }
        fn submit(
            &mut self,
            operation: fr_core::input_submission::Operation,
        ) -> fr_core::input_submission::Submission {
            self.effects.lock().unwrap().push(operation);
            fr_core::input_submission::Submission::Submitted
        }
    }
    use fr_core::{
        input::{InputEvent, InputRequest, KeyTransition, PhysicalKey},
        input_submission::{Dispatch, Operation},
    };
    use fr_wire::input::{InputDelivery, InputDirection, encode_input};
    use frd::input_agent::{Reply, Route, Seat};
    async fn input_scenario(fixture: &Fixture, cx: Cx, addresses: ConnectionAddresses, case: u8) {
        let proof = fixture
            .api
            .authorize_app_capability(&cx, addresses, GrantPolicy::default())
            .await
            .unwrap();
        let owner = Admission::new(fixture.api.clone(), cx.clone(), proof).unwrap();
        let gate = owner.lease();
        let seat = Seat::default();
        let effects = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = effects.clone();
        let constructed = Arc::new(AtomicBool::new(false));
        let made = constructed.clone();
        let sink_gate = gate.clone();
        let started = seat.start_admitted(
            cx.clone(),
            input_session(&cx),
            Route::new(7, ProtocolLimits::ABSOLUTE),
            gate.clone(),
            move || {
                made.store(true, Ordering::Release);
                Ok(RecordingInput {
                    effects: recorded,
                    gate: sink_gate,
                    revoke_in_prepare: case == 2,
                })
            },
            |_| true,
        );
        if case == 0 {
            assert!(matches!(
                started,
                Err(frd::input_agent::Error::Admission(
                    fr_tailnet::Error::CapabilityDenied
                ))
            ));
            assert!(!constructed.load(Ordering::Acquire));
            assert!(!seat.is_occupied());
            assert!(gate.observe().is_ok());
            return;
        }
        let (mut agent, driver) = started.unwrap();
        let key = PhysicalKey::new(4).unwrap();
        let mut bytes = [0; 256];
        let count = encode_input(
            InputRequest {
                credentials: input_credentials(),
                sequence: 0,
                event: InputEvent::Key {
                    key,
                    transition: KeyTransition::Press,
                },
            },
            &mut bytes,
            &ProtocolLimits::ABSOLUTE,
            7,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        agent
            .submit(&bytes[..count], InputDelivery::Reliable)
            .unwrap();
        let reply = agent.response().await.unwrap();
        if case == 1 {
            assert!(matches!(reply, Reply::Input(Ok(Dispatch::Completed(_)))));
            assert!(effects.lock().unwrap().contains(&Operation::Key {
                key,
                transition: KeyTransition::Press
            }));
            owner.revoke();
        } else {
            assert!(!effects.lock().unwrap().iter().any(|op| matches!(
                op,
                Operation::Key {
                    transition: KeyTransition::Press,
                    ..
                }
            )));
        }
        let shutdown = driver.await;
        assert!(
            shutdown
                .exit
                .is_some_and(frd::input_agent::Exit::handoff_safe)
        );
        assert!(!seat.is_occupied());
        if case == 1 {
            assert!(effects.lock().unwrap().contains(&Operation::Key {
                key,
                transition: KeyTransition::Release
            }));
        }
        assert!(gate.control().is_err());
        assert!(
            agent
                .submit(&bytes[..count], InputDelivery::Reliable)
                .is_err()
        );
    }
    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        if UnixStream::pair()?.0.peer_cred()?.uid != 0 {
            return Err("run this opt-in synthetic fixture as root; no production peer-credential bypass exists".into());
        }
        let fixture = Fixture::new();
        let runtime = RuntimeBuilder::new()
            .worker_threads(2)
            .enable_platform_reactor(true)
            .build()?;
        let addresses = ConnectionAddresses {
            local: "100.64.0.1:4710".parse()?,
            peer: "100.64.0.2:30001".parse()?,
        };
        for case in 0..5 {
            let cx = runtime.request_cx_with_budget(Budget::INFINITE);
            fixture.denied.store(false, Ordering::Release);
            runtime.block_on(scenario(&fixture, cx, addresses, case));
        }
        for case in 0..3 {
            fixture.denied.store(false, Ordering::Release);
            fixture.input_allowed.store(case != 0, Ordering::Release);
            let cx = runtime.request_cx_with_budget(Budget::INFINITE);
            runtime.block_on(input_scenario(&fixture, cx, addresses, case));
        }
        println!(
            "PASS: 5 media and 3 input LocalAPI admission scenarios; real root peer credentials, synthetic metadata/recording input sink, no live tailnet/codec/OS-input claim"
        );
        Ok(())
    }
}
