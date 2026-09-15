//! Public native observer waits in a read-only state until one local control decision.
//! UDP/TLS and worker supervision are real; codec, visibility and OS effects are fixtures.
use super::*;

#[test]
#[allow(clippy::too_many_lines)]
fn public_native_viewer_watches_past_request_budget_then_takes_control_in_place() {
    run3(|c, h, cleanup| async move {
        let (mut host, viewer) = pair_initialized(&c, &h, capabilities(), |_| {}).await;
        let authority = host.observation().unwrap();
        let mut nonce_counter = 30_000u128;
        let (published, observed) = Box::pin(support::both(
            host.publish_controlled_display(
                launch(WorkerRole::Capture),
                PublisherPolicy::default(),
                config,
                || {
                    nonce_counter += 1;
                    Ok(nonce_counter)
                },
            ),
            viewer.observe_for_control(
                launch(WorkerRole::Present),
                ObserverPolicy::default(),
                ClockPolicy::default(),
                |catalog| Ok(Some(catalog.displays()[0].handle)),
            ),
        ))
        .await;
        let mut host = published.unwrap();
        let mut viewer = observed.unwrap();
        let caps = Capabilities::default().with(Capability::Keys);
        let target = host.control_target(caps).unwrap();
        let request = viewer.control_request(41, caps).unwrap();
        let host_pid = host.worker_id();
        let viewer_pid = viewer.worker_id();
        let stop = viewer.control();
        let seat = Seat::default();
        let viewer_seat = seat.clone();
        let effects = Arc::new(Mutex::new(Vec::new()));
        let (tx, mut rx) = asupersync::channel::mpsc::channel::<Driver>(1);
        let host_done = Cell::new(false);
        let viewer_done = Cell::new(false);
        let request_started = Cell::new(false);
        let host_saw_request = Cell::new(false);
        let viewing_turns = Cell::new(0u64);
        let receipts = Cell::new(0u64);
        let mut shown = None;
        let mut sent_actions = 0u8;
        let mut activated = false;
        let mut ticket = 40_000u128;
        let start = now(&c).unwrap();
        let ((host_result, viewer_result), ()) = Box::pin(support::both(
            support::both(
                async {
                    let result = host
                        .serve_accepting_control(
                            seat.clone(),
                            |state| {
                                if let HostControlState::Pending(mut pending) = state {
                                    if pending.request().is_none() {
                                        assert!(!request_started.get());
                                        assert!(effects.lock().unwrap().is_empty());
                                    } else {
                                        assert!(request_started.get());
                                        host_saw_request.set(true);
                                        if pending.native_status().is_none() && pending.view_ready()? {
                                            let sink_effects = effects.clone();
                                            let driver = pending.approve(
                                                target,
                                                || {
                                                    Some((
                                                        InputLeaseId::from_raw(91),
                                                        InputTicketId::from_raw(92),
                                                    ))
                                                },
                                                move || Ok(Sink(sink_effects)),
                                                |_| true,
                                            )?;
                                            assert!(tx.try_send(driver).is_ok());
                                        }
                                    }
                                }
                                Ok(Some(target))
                            },
                            || {
                                nonce_counter += 1;
                                Ok(nonce_counter)
                            },
                            || {
                                ticket += 1;
                                Some(InputTicketId::from_raw(ticket))
                            },
                        )
                        .await;
                    host_done.set(true);
                    stop.stop();
                    result
                },
                async {
                    let result = viewer
                        .serve_interactive_control(
                            41,
                            caps,
                            fr_client::input::Policy::default(),
                            |state, frame| {
                                match state {
                                    session_startup::InteractiveViewerState::Viewing(viewing) => {
                                        viewing_turns.set(viewing_turns.get() + 1);
                                        assert!(!viewer_seat.is_occupied());
                                        assert_eq!(viewing.request(), request);
                                        viewing
                                            .confirm_mapping(request.parent, request.target.view)
                                            .unwrap();
                                        if let Some(p) = viewing.presentation()
                                            && shown != Some(p.frame)
                                        {
                                            viewing.visible(p.frame.as_raw()).unwrap();
                                            shown = Some(p.frame);
                                        }
                                        if now(&c).unwrap() >= start + 2_200_000
                                            && !request_started.get()
                                        {
                                            assert!(viewing.evidence().is_ok());
                                            let before = now(&c).unwrap();
                                            let deadline = viewing.request_control().unwrap();
                                            assert!(deadline.0 >= before + 1_900_000);
                                            assert!(matches!(
                                                viewing.request_control(),
                                                Err(session_startup::StreamingViewerError::RequestAlreadyStarted)
                                            ));
                                            request_started.set(true);
                                        }
                                    }
                                    session_startup::InteractiveViewerState::Requesting(pending) => {
                                        assert!(request_started.get());
                                        assert_eq!(pending.request(), request);
                                    }
                                    session_startup::InteractiveViewerState::Controlled(input) => {
                                        assert!(request_started.get());
                                        activated = true;
                                        if let Some(p) = frame {
                                            input.visible(p.frame.as_raw()).unwrap();
                                        }
                                        if sent_actions == 0
                                            || (sent_actions == 1 && receipts.get() == 1)
                                        {
                                            let _ = input
                                                .action(fr_client::input::Action::Key {
                                                    key: PhysicalKey::new(4).unwrap(),
                                                    transition: if sent_actions == 0 {
                                                        KeyTransition::Press
                                                    } else {
                                                        KeyTransition::Release
                                                    },
                                                })
                                                .unwrap();
                                            sent_actions += 1;
                                        }
                                        if receipts.get() == 2 {
                                            authority.revoke();
                                            stop.stop();
                                        }
                                    }
                                }
                                Ok(())
                            },
                            |_| receipts.set(receipts.get() + 1),
                        )
                        .await;
                    viewer_done.set(true);
                    authority.revoke();
                    result
                },
            ),
            async {
                let mut driver = None;
                let mut done = false;
                loop {
                    if driver.is_none()
                        && let Ok(value) = rx.try_recv()
                    {
                        driver = Some(value);
                    }
                    if let Some(value) = &mut driver {
                        std::future::poll_fn(|task| {
                            if !done && let Poll::Ready(result) = Pin::new(&mut *value).poll(task) {
                                assert!(result.handoff_safe());
                                done = true;
                            }
                            Poll::Ready(())
                        })
                        .await;
                    }
                    if host_done.get() && viewer_done.get() && (driver.is_none() || done) {
                        break;
                    }
                    asupersync::time::sleep(cleanup.now(), Duration::from_millis(1)).await;
                }
            },
        ))
        .await;
        assert!(host_result.is_err() && viewer_result.is_err());
        assert!(request_started.get());
        assert!(host_saw_request.get());
        assert!(viewing_turns.get() > 3);
        assert!(activated);
        assert_eq!(sent_actions, 2);
        assert_eq!(receipts.get(), 2);
        assert_eq!(effects.lock().unwrap().len(), 2);
        assert_eq!(host.worker_id(), host_pid);
        assert_eq!(viewer.worker_id(), viewer_pid);
        assert_eq!(host.statistics().encoded_updates, 0);
        assert_eq!(viewer.statistics().decoded, 0, "initial frame decoded again");
        assert!(host.presentation_reports() > 3);
        assert!(viewer.presentation_reports() > 3);
        assert!(!seat.is_occupied());
        host.reap_media(
            &cleanup,
            Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        viewer
            .reap_media(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
    });
}
