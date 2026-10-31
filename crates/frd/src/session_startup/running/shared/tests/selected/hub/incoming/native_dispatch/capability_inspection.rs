//! Actual TLS/UDP and original dispatcher ownership. Identity, local permission,
//! monitor pixels, HEVC and decoder acknowledgement remain explicit fixtures.
use super::*;
use fr_client::startup::Startup;

struct Probe {
    c: Cx,
    h: Cx,
    host: Host,
    native: support::NativeResult,
    offer: Offer,
    alive: Arc<AtomicBool>,
}
async fn probe(rt: &Runtime, id: u128) -> Probe {
    let c = rt.request_cx_with_budget(Budget::INFINITE);
    let h = rt.request_cx_with_budget(Budget::INFINITE);
    let offer = fr_wire::inspection::prepare_offer(Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: vec![],
    })
    .unwrap();
    let mut advertised = offer.clone();
    for capability in &mut advertised.capabilities {
        capability.required = false;
    }
    let config = Configuration {
        offer: advertised,
        binding: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(id),
        },
        require_approval: true,
        startup_timeout: Duration::from_secs(4),
        authority: AuthorityPolicy::plan_defaults(),
        transport: Policy {
            critical_send_records: 1,
            ..Policy::default()
        },
    };
    let (native, server) = support::native_pair(&c, "localhost", quic::ALPN).await;
    let alive = Arc::new(AtomicBool::new(true));
    let peer = Peer::Fixture {
        alive: alive.clone(),
        until: now(&h).unwrap() + 30_000_000,
        control: false,
    };
    let host = Host::start(h.clone(), server.unwrap(), peer, config).unwrap();
    Probe {
        c,
        h,
        host,
        native,
        offer,
        alive,
    }
}
async fn inspect(incoming: &Incoming, rt: &Runtime, id: u128) {
    let Probe {
        c,
        h,
        host,
        native,
        offer,
        alive,
    } = probe(rt, id).await;
    let connection = incoming
        .serve_host(host, |_, _| panic!("inspection must not request approval"))
        .unwrap();
    let viewer = Viewer::new(
        c,
        native.unwrap(),
        offer.clone(),
        Policy::default(),
        Duration::from_secs(4),
    )
    .unwrap();
    let (result, ended) = Box::pin(support::both(viewer.inspect_capabilities(), connection)).await;
    let advertised = result.unwrap();
    offer.check_host(&advertised).unwrap();
    assert_eq!(ended, Ok(()));
    assert!(!alive.load(Ordering::Acquire));
    assert!(h.is_cancel_requested(), "original inspection peer retired");
}

#[test]
#[allow(clippy::too_many_lines)]
fn repeated_inspections_leave_the_original_factory_for_a_later_approved_session() {
    let rt = support::runtime();
    rt.block_on(Box::pin(async {
        let (incoming, mut driver, independent) = pair(&rt);
        let (setup, source, mut retirement, trace) = preparation::setup(&rt, "normal");
        let factories = Arc::new(AtomicU64::new(0));
        let choices = Arc::new(AtomicU64::new(0));
        let counted = factories.clone();
        let chosen = choices.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let local_stop = stop.clone();
        let mut running = Box::pin(driver.serve_async(
            move || async move {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(setup)
            },
            move |catalog| {
                chosen.fetch_add(1, Ordering::SeqCst);
                preparation::choose(catalog)
            },
            move |_, _| {
                Ok(if local_stop.load(Ordering::Acquire) {
                    LocalAction::Stop
                } else {
                    LocalAction::Continue
                })
            },
        ));
        is_send(&running);
        let mut clients = Box::pin(async {
            for id in [101, 102] {
                inspect(&incoming, &rt, id).await;
                assert_eq!(factories.load(Ordering::SeqCst), 0);
                assert_eq!(choices.load(Ordering::SeqCst), 0);
                assert!(!trace.exists(), "no worker or discovery during inspection");
                assert!(!independent.is_cancel_requested());
            }
            let Fresh {
                c,
                h,
                host,
                mut viewer,
                ..
            } = *fresh(&rt, 103, true, Role::Observe, Duration::from_secs(4)).await;
            let notice = Arc::new(Mutex::new(None));
            let saved = notice.clone();
            let connection = incoming
                .serve_host(host, move |approval, role| {
                    assert_eq!(role, Role::Observe);
                    assert!(saved.lock().unwrap().replace(approval).is_none());
                    Ok(())
                })
                .unwrap();
            prompt(&mut viewer, &notice).await;
            assert_eq!(factories.load(Ordering::SeqCst), 0);
            assert_eq!(choices.load(Ordering::SeqCst), 0);
            assert!(!trace.exists(), "a probe did not confer later consent");
            notice.lock().unwrap().take().unwrap().decide(true).unwrap();
            let mut viewer = Box::pin(Client::start(c, viewer)).await;
            viewer.ready().await;
            assert!(!viewer.frames.is_empty());
            assert_eq!(factories.load(Ordering::SeqCst), 1);
            assert_eq!(choices.load(Ordering::SeqCst), 1);
            assert!(!h.is_cancel_requested());
            assert!(!independent.is_cancel_requested());
            stop.store(true, Ordering::Release);
            std::future::pending::<()>().await;
            drop(connection);
        });
        let report = poll_fn(|task| {
            if let Poll::Ready(result) = running.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(clients.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await
        .unwrap();
        assert_eq!(report.viewers.admitted, 1, "probes were not observations");
        assert!(source.check().is_err());
        drop(clients);
        drop(running);
        reap_driver(&mut driver, &mut retirement).await;
    }));
}

/// Send the inspection hello and validate its capabilities, then deliberately
/// abandon before sending the close record. The real host must retire this
/// recognized probe under its original two-second budget, without a source.
async fn abandon(incoming: &Incoming, rt: &Runtime) {
    let Probe {
        c,
        h,
        host,
        native,
        offer,
        alive,
    } = probe(rt, 201).await;
    let connection = incoming
        .serve_host(host, |_, _| {
            panic!("abandoned probe must not request approval")
        })
        .unwrap();
    let leave = async {
        let (mut transport, routes) =
            fr_transport::quic::QuicRecords::bootstrap(native.unwrap(), &c, Policy::default())
                .unwrap();
        let started = now(&c).unwrap();
        let mut startup = Startup::new(offer, started, 4_000_000).unwrap();
        startup.begin_inspection(started).unwrap();
        transport
            .send(
                &c,
                Route::Stream(routes.outbound),
                startup.pending(started).unwrap().unwrap(),
                startup.deadline_us(),
                || true,
            )
            .unwrap();
        startup.sent(started).unwrap();
        let received = std::cell::Cell::new(false);
        while !received.get() {
            transport
                .drive(&c, Duration::from_millis(1), || true)
                .await
                .unwrap();
            transport
                .receive_ready(
                    &c,
                    || true,
                    |_| !received.get(),
                    |route, bytes| {
                        assert_eq!(route, Route::Stream(routes.inbound));
                        startup.receive(bytes, now(&c).unwrap()).unwrap();
                        received.set(true);
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        assert!(!startup.is_inspected());
        assert!(
            startup.pending(now(&c).unwrap()).unwrap().is_some(),
            "the close record is deliberately not sent"
        );
        transport.close();
    };
    let ((), ended) = Box::pin(support::both(leave, connection)).await;
    assert_eq!(
        ended,
        Err(DispatchError::Startup(OpenError::InspectionAbandoned))
    );
    assert!(!alive.load(Ordering::Acquire));
    assert!(h.is_cancel_requested());
}

#[test]
fn abandoned_inspection_retires_one_peer_and_local_stop_still_fences_the_rearmed_slot() {
    let rt = support::runtime();
    rt.block_on(Box::pin(async {
        let (incoming, mut driver, independent) = pair(&rt);
        let stop = Arc::new(AtomicBool::new(false));
        let local_stop = stop.clone();
        let mut running = Box::pin(driver.serve(
            || panic!("neither inspection may start a source"),
            |_| panic!("neither inspection may select a display"),
            move |_, _| {
                Ok(if local_stop.load(Ordering::Acquire) {
                    LocalAction::Stop
                } else {
                    LocalAction::Continue
                })
            },
        ));
        let mut clients = Box::pin(async {
            abandon(&incoming, &rt).await;
            inspect(&incoming, &rt, 202).await;
            assert!(!independent.is_cancel_requested());
            stop.store(true, Ordering::Release);
            std::future::pending::<()>().await;
        });
        let result = poll_fn(|task| {
            if let Poll::Ready(result) = running.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(clients.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await;
        assert_eq!(result, Err(DispatchError::Closed));
        drop(clients);
        drop(running);
        assert!(driver.worker_id().is_none());
        assert!(!independent.is_cancel_requested());
        let Fresh {
            host,
            h,
            viewer: _viewer,
            ..
        } = *fresh(&rt, 203, true, Role::Observe, Duration::from_secs(4)).await;
        assert!(matches!(
            incoming.serve_host(host, |_, _| panic!("closed original owner")),
            Err(DispatchError::Closed)
        ));
        assert!(h.is_cancel_requested());
    }));
}
