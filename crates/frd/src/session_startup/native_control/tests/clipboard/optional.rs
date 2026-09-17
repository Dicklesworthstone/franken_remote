//! Capability selection is not local clipboard consent. A native application
//! without a configured clipboard must explicitly decline, not strand its peer.
use super::*;

#[allow(clippy::too_many_lines)]
async fn unconfigured_peer(c: Cx, h: Cx, cleanup: Cx, configured_ends: [bool; 2]) {
    let (mut host, viewer) = pair_initialized(&c, &h, caps(), |_| {}).await;
    let authority = host.observation().unwrap();
    let mut nonce = 100_000u128;
    let (host, viewer) = Box::pin(support::both(
        host.publish_controlled_display(
            launch(WorkerRole::Capture),
            PublisherPolicy::default(),
            config,
            || {
                nonce += 1;
                Ok(nonce)
            },
        ),
        viewer.observe_for_control(
            launch(WorkerRole::Present),
            ObserverPolicy::default(),
            ClockPolicy::default(),
            |cat| Ok(Some(cat.displays()[0].handle)),
        ),
    ))
    .await;
    let (mut host, mut viewer) = (host.unwrap(), viewer.unwrap());
    let workers = (host.worker_id(), viewer.worker_id());
    let os = [
        Arc::new(Mutex::new(Os::default())),
        Arc::new(Mutex::new(Os::default())),
    ];
    let host_control = configured_ends[0].then(|| {
        host.configure_clipboard(configured(&os[0], true, None))
            .unwrap()
    });
    let viewer_control = configured_ends[1].then(|| {
        viewer
            .configure_clipboard(configured(&os[1], true, None))
            .unwrap()
    });
    let keys = Capabilities::default().with(Capability::Keys);
    let target = host.control_target(keys).unwrap();
    let request = viewer.control_request(7, keys).unwrap();
    let stop = viewer.control();
    let seat = Seat::default();
    let effects = Arc::new(Mutex::new(Vec::new()));
    let mut ticket = 150_000u128;
    let receipts = Cell::new(0);
    let start = now(&c).unwrap();
    let mut shown = None;
    let mut requested = false;
    let mut actions = 0;
    let mut complete = false;
    let (hr, vr) = Box::pin(support::both(
        async {
            let result = host
                .serve_managed_control(
                    seat.clone(),
                    |state| {
                        if let ManagedHostControlState::Pending(mut pending) = state
                            && pending.request().is_some()
                            && pending.native_status().is_none()
                            && pending.view_ready()?
                        {
                            let effects = effects.clone();
                            pending.approve(
                                target,
                                || Some((InputLeaseId::from_raw(91), InputTicketId::from_raw(92))),
                                move || Ok(Sink(effects)),
                                |_| true,
                            )?;
                        }
                        Ok(Some(target))
                    },
                    || {
                        nonce += 1;
                        Ok(nonce)
                    },
                    || {
                        ticket += 1;
                        Some(InputTicketId::from_raw(ticket))
                    },
                )
                .await;
            stop.stop();
            result
        },
        async {
            let result = viewer
                .serve_interactive_control(
                    7,
                    keys,
                    fr_client::input::Policy::default(),
                    |state, frame| {
                        assert!(
                            now(&c).unwrap() < start + 4_000_000,
                            "unconfigured clipboard stranded the peer"
                        );
                        assert!(
                            !os[0].lock().unwrap().entered && !os[1].lock().unwrap().entered,
                            "missing native configuration is not consent"
                        );
                        match state {
                            session_startup::InteractiveViewerState::Viewing(viewing) => {
                                viewing
                                    .confirm_mapping(request.parent, target.view)
                                    .unwrap();
                                if let Some(p) = viewing.presentation()
                                    && shown != Some(p.frame)
                                {
                                    viewing.visible(p.frame.as_raw()).unwrap();
                                    shown = Some(p.frame);
                                }
                                if !requested && now(&c).unwrap() > start + 100_000 {
                                    viewing.request_control().unwrap();
                                    requested = true;
                                }
                            }
                            session_startup::InteractiveViewerState::Requesting(_) => {}
                            session_startup::InteractiveViewerState::Controlled(input) => {
                                if let Some(p) = frame {
                                    input.visible(p.frame.as_raw()).unwrap();
                                }
                                // Application status is collected on each endpoint's
                                // own turn, after its actual readiness/retirement event.
                                let local_notified = [&host_control, &viewer_control]
                                    .into_iter()
                                    .flatten()
                                    .all(|control| control.status().phase == Phase::Retired);
                                if input.clipboard_retired() && local_notified && actions == 0 {
                                    for control in
                                        [&host_control, &viewer_control].into_iter().flatten()
                                    {
                                        assert_eq!(
                                            control.status().reason,
                                            Some(ClipboardError::ConsentRequired)
                                        );
                                        assert_eq!(control.status().cleanup, Cleanup::NotStarted);
                                        assert_eq!(
                                            control.set_enabled(true),
                                            Err(ClipboardError::Closed)
                                        );
                                    }
                                    let _ = input
                                        .action(fr_client::input::Action::Key {
                                            key: PhysicalKey::new(4).unwrap(),
                                            transition: KeyTransition::Press,
                                        })
                                        .unwrap();
                                    actions = 1;
                                }
                                if actions == 1 && receipts.get() == 1 {
                                    let _ = input
                                        .action(fr_client::input::Action::Key {
                                            key: PhysicalKey::new(4).unwrap(),
                                            transition: KeyTransition::Release,
                                        })
                                        .unwrap();
                                    actions = 2;
                                }
                                if actions == 2 && receipts.get() == 2 {
                                    complete = true;
                                    stop.stop();
                                    authority.revoke();
                                }
                            }
                        }
                        Ok(())
                    },
                    |_| receipts.set(receipts.get() + 1),
                )
                .await;
            authority.revoke();
            result
        },
    ))
    .await;
    assert!(
        complete,
        "optional refusal must preserve keyboard control: {hr:?}, {vr:?}"
    );
    assert!(requested && hr.session.is_err() && vr.is_err());
    assert!(hr.input.unwrap().handoff_safe());
    assert!(!seat.is_occupied());
    assert_eq!(effects.lock().unwrap().len(), 2);
    assert_eq!((host.worker_id(), viewer.worker_id()), workers);
    let deadline = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
    assert_eq!(
        host.reap_clipboard(&cleanup, deadline).await.unwrap(),
        Cleanup::NotStarted
    );
    assert_eq!(
        viewer.reap_clipboard(&cleanup, deadline).await.unwrap(),
        Cleanup::NotStarted
    );
    for os in &os {
        let os = os.lock().unwrap();
        assert!(!os.entered && !os.opened && !os.closed && os.published.is_empty());
    }
    host.reap_media(&cleanup, deadline).await.unwrap();
    viewer.reap_media(&cleanup, deadline).await.unwrap();
}

#[test]
fn missing_host_clipboard_configuration_declines_without_ending_control() {
    run3(|c, h, k| Box::pin(unconfigured_peer(c, h, k, [false, true])));
}
#[test]
fn missing_viewer_clipboard_configuration_declines_without_ending_control() {
    run3(|c, h, k| Box::pin(unconfigured_peer(c, h, k, [true, false])));
}
#[test]
fn missing_both_clipboard_configurations_never_open_the_os_or_strand_control() {
    run3(|c, h, k| Box::pin(unconfigured_peer(c, h, k, [false, false])));
}
