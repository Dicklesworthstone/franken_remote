//! Real running peers and their original TLS/UDP, media subprocesses and grant
//! broker. Capture/codec replies, visibility, consent and the OS sink are test
//! fixtures, not hardware or native desktop qualification.
use super::*;
use crate::{
    input_agent::{Driver, Seat},
    media::PresentationStage,
    session_startup::{HostControlState, InteractiveViewerState},
};
use fr_client::input::{Action, Policy as InputPolicy};
use fr_core::{
    input::{DesktopPoint, InputBounds, InputView, KeyTransition, PhysicalKey},
    input_submission::{Capabilities, Capability, InputSink, Operation, PlatformError, Submission},
};
use fr_wire::{
    control::{Request, Target},
    decoder::Binding,
};
use std::{cell::Cell, pin::Pin, sync::Mutex, task::Poll};

struct Sink(Arc<Mutex<Vec<Operation>>>);
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, operation: Operation) -> Submission {
        self.0.lock().unwrap().push(operation);
        Submission::Submitted
    }
}
fn target(binding: Binding) -> Target {
    Target {
        display_binding: binding.parent.id,
        view: InputView {
            geometry: binding.geometry,
            configuration: binding.configuration,
            recovery: binding.recovery,
            viewport: binding.viewport,
        },
        bounds: InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        capabilities: Capabilities::default().with(Capability::Keys),
    }
}
fn key(pressed: bool) -> Action<'static> {
    Action::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: if pressed {
            KeyTransition::Press
        } else {
            KeyTransition::Release
        },
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn pre_request_viewing_recovers_without_authority_then_grants_only_the_current_view() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (
            Fixture {
                mut host,
                viewer,
                media,
                receiver,
                presenter,
                initial,
            },
            input,
        ) = Box::pin(fixture_with_input(
            &c,
            &h,
            "healthy",
            2_000_000,
            Role::RequestControl,
            true,
            true,
            true,
        ))
        .await;
        let (hi, vi) = input.unwrap();
        let request = Request {
            parent: viewer.metadata().binding,
            sequence: 1,
            target: target(media.binding()),
        };
        let original_host = host.worker_id();
        let original_viewer = presenter.worker_id();
        let original_connection = host.host.session().unwrap().opened.transport.binding();
        let old_frame = initial.frame.as_raw();
        let mut viewer =
            StreamingViewer::from_test_parts(viewer, media, presenter, receiver, initial);
        // The fixture lost an actual reference fragment. Silence exhausts its
        // original useful-reference budget before ordinary repair can save it.
        asupersync::time::sleep(c.now(), Duration::from_millis(150)).await;
        let stop_host = host.stream.control.clone();
        let stop_viewer = viewer.control();
        let seat = Seat::default();
        let effects = Arc::new(Mutex::new(Vec::new()));
        let (drivers, mut incoming) = asupersync::channel::mpsc::channel::<Driver>(1);
        let host_done = Cell::new(false);
        let viewer_done = Cell::new(false);
        let decision = Cell::new(None);
        let recovered_target = Cell::new(None);
        let receipts = Cell::new(0);
        let native_calls = Arc::new(AtomicU64::new(0));
        let start = now(&c).unwrap();
        let mut entropy = 9000;
        let mut ticket = 1000;
        let mut idle_turns = 0;
        let mut host_recovering = false;
        let mut viewer_recovering = false;
        let mut recovered = false;
        let mut mapped = false;
        let mut visible = None;
        let mut granted = false;
        let mut sent = 0;
        let ((host_result, viewer_result), ()) = Box::pin(support::both(
            support::both(
                async {
                    let result = host.serve_accepting_control(
                        seat.clone(), hi,
                        |state| match state {
                            HostControlState::Pending(mut pending) => {
                                if decision.get().is_none() {
                                    assert!(pending.request().is_none());
                                    assert!(pending.native_status().is_none());
                                    assert!(!seat.is_occupied());
                                    assert_eq!(native_calls.load(Ordering::SeqCst), 0);
                                    idle_turns += 1;
                                }
                                let Some(binding) = pending.configuration()? else {
                                    host_recovering = true;
                                    assert!(!pending.view_ready()?);
                                    return Ok(None);
                                };
                                let current = target(binding);
                                if let Some(received) = pending.request() {
                                    assert!(decision.get().is_some());
                                    assert_eq!(received, Request { target: current, ..request });
                                    assert_eq!(Some(current), recovered_target.get());
                                    if pending.native_status().is_none() && pending.view_ready()? {
                                        let effects = effects.clone();
                                        let calls = native_calls.clone();
                                        let driver = pending.approve(
                                            current,
                                            || Some((InputLeaseId::from_raw(19), InputTicketId::from_raw(23))),
                                            move || {
                                                calls.fetch_add(1, Ordering::SeqCst);
                                                Ok(Sink(effects))
                                            },
                                            |_| true,
                                        )?;
                                        assert!(drivers.try_send(driver).is_ok());
                                    }
                                }
                                Ok(Some(current))
                            }
                            HostControlState::Active { request: active, .. } => {
                                assert!(decision.get().is_some());
                                assert_eq!(Some(active.target), recovered_target.get());
                                granted = true;
                                Ok(Some(active.target))
                            }
                        },
                        || super::super::super::tests::nonce(&mut entropy),
                        || { ticket += 1; Some(InputTicketId::from_raw(ticket)) },
                        super::super::super::tests::block,
                    ).await;
                    host_done.set(true);
                    result
                },
                async {
                    let result = viewer.serve_interactive_control(
                        vi, request, InputPolicy::default(),
                        |state, event| {
                            match state {
                                InteractiveViewerState::Viewing(view) => {
                                    assert!(decision.get().is_none());
                                    assert!(!seat.is_occupied());
                                    if !mapped {
                                        view.confirm_mapping(request.parent, request.target.view).unwrap();
                                        mapped = true;
                                    }
                                    if view.presentation().is_none() {
                                        viewer_recovering = true;
                                        assert!(view.evidence().is_err());
                                        assert!(view.visible(old_frame).is_err());
                                        assert!(view.confirm_mapping(request.parent, request.target.view).is_err());
                                        assert!(view.request_control().is_err());
                                    } else if view.request().target != request.target {
                                        let current = view.request();
                                        assert_eq!(current.parent, request.parent);
                                        assert_eq!(current.sequence, request.sequence);
                                        assert_eq!(current.target.bounds, request.target.bounds);
                                        assert_eq!(current.target.capabilities, request.target.capabilities);
                                        assert_eq!(current.target.view, InputView {
                                            recovery: request.target.view.recovery.next().unwrap(),
                                            ..request.target.view
                                        });
                                        assert_ne!(current.target.display_binding, request.target.display_binding);
                                        assert!(view.confirm_mapping(request.parent, request.target.view).is_err());
                                        if !recovered {
                                            // A decoded successor is not visible evidence. A late
                                            // callback from the old generation cannot qualify it.
                                            assert!(view.evidence().is_err());
                                            assert!(view.visible(old_frame).is_err());
                                            recovered = true;
                                            recovered_target.set(Some(current.target));
                                        }
                                        if let Some(frame) = view.presentation()
                                            && visible != Some(frame.frame)
                                        {
                                            assert_eq!(frame.stage, PresentationStage::SubmittedToCompositor);
                                            crate::session_startup::confirm_visible(view.visible(frame.frame.as_raw()));
                                            visible = Some(frame.frame);
                                        }
                                        // Deliberately watch beyond a complete request lifetime.
                                        // No old deadline may have started at entry or recovery.
                                        if now(&c).unwrap() > start + 2_250_000 && view.evidence().is_ok() {
                                            let before = now(&c).unwrap();
                                            let until = view.request_control().unwrap();
                                            assert!(until.0 >= before + fr_client::control_grant::REQUEST_LIFETIME_US);
                                            decision.set(Some(until));
                                        }
                                    }
                                }
                                InteractiveViewerState::Requesting(pending) => {
                                    assert_eq!(pending.deadline(), decision.get().unwrap());
                                    assert_eq!(Some(pending.request().target), recovered_target.get());
                                    // No second mapping acknowledgement: the exact reference-only
                                    // successor retained the original physical coordinate map.
                                    if let Some(frame) = pending.presentation()
                                        && visible != Some(frame.frame)
                                    {
                                        crate::session_startup::confirm_visible(pending.visible(frame.frame.as_raw()));
                                        visible = Some(frame.frame);
                                    }
                                }
                                InteractiveViewerState::Controlled(controlled) => {
                                    if let Some(frame) = event {
                                        crate::session_startup::confirm_visible(controlled.visible(frame.frame.as_raw()));
                                    }
                                    if sent == 0 {
                                        controlled.action(key(true)).unwrap();
                                        sent = 1;
                                    } else if sent == 1 && receipts.get() >= 1 {
                                        controlled.action(key(false)).unwrap();
                                        sent = 2;
                                    }
                                    if receipts.get() == 2 {
                                        stop_host.revoke();
                                        stop_viewer.stop();
                                    }
                                }
                            }
                            if host_done.get() { stop_viewer.stop(); }
                            Ok(())
                        },
                        |_| receipts.set(receipts.get() + 1),
                        super::super::super::tests::block,
                    ).await;
                    viewer_done.set(true);
                    stop_host.revoke();
                    result
                },
            ),
            async {
                let mut driver = None;
                let mut completed = false;
                loop {
                    if driver.is_none() && let Ok(received) = incoming.try_recv() {
                        driver = Some(received);
                    }
                    if let Some(driver) = &mut driver {
                        std::future::poll_fn(|task| {
                            if !completed && let Poll::Ready(result) = Pin::new(&mut *driver).poll(task) {
                                assert!(result.handoff_safe());
                                completed = true;
                            }
                            Poll::Ready(())
                        }).await;
                    }
                    if host_done.get() && viewer_done.get() && (driver.is_none() || completed) {
                        break;
                    }
                    asupersync::time::sleep(h.now(), Duration::from_millis(1)).await;
                }
            },
        )).await;
        assert!(host_result.is_err() && viewer_result.is_err());
        assert!(
            mapped && viewer_recovering && host_recovering && recovered && granted,
            "host={host_result:?} viewer={viewer_result:?}"
        );
        assert!(idle_turns > 1);
        assert_eq!(native_calls.load(Ordering::SeqCst), 1);
        assert_eq!(receipts.get(), 2);
        assert_eq!(effects.lock().unwrap().len(), 2);
        assert!(!seat.is_occupied());
        assert_eq!(host.worker_id(), original_host);
        assert_eq!(viewer.worker_id(), original_viewer);
        assert!(
            host.host
                .session()
                .unwrap()
                .opened
                .transport
                .is_bound_to(&original_connection)
        );
        assert_eq!(host.statistics().recovered_streams, 1);
        assert_eq!(viewer.statistics().recovered_streams, 1);
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

#[derive(Clone, Copy)]
enum Terminal {
    PendingRequest,
    LocalStop,
}

#[test]
fn an_explicit_pending_request_is_not_rewritten_by_viewing_recovery() {
    terminal(Terminal::PendingRequest);
}

#[test]
fn local_stop_during_pre_request_recovery_closes_the_original_owners_without_a_grant() {
    terminal(Terminal::LocalStop);
}

#[allow(clippy::too_many_lines)]
fn terminal(case: Terminal) {
    run(move |c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (
            Fixture {
                mut host,
                viewer,
                media,
                receiver,
                presenter,
                initial,
            },
            input,
        ) = Box::pin(fixture_with_input(
            &c,
            &h,
            "healthy",
            2_000_000,
            Role::RequestControl,
            true,
            true,
            true,
        ))
        .await;
        let (hi, vi) = input.unwrap();
        let request = Request {
            parent: viewer.metadata().binding,
            sequence: 1,
            target: target(media.binding()),
        };
        let mut viewer =
            StreamingViewer::from_test_parts(viewer, media, presenter, receiver, initial);
        asupersync::time::sleep(c.now(), Duration::from_millis(150)).await;
        let stop = host.stream.control.clone();
        let seat = Seat::default();
        let mut callback = false;
        let mut terminal = false;
        let mut entropy = 12_000;
        let (host_result, viewer_result) = Box::pin(support::both(
            host.serve_accepting_control(
                seat.clone(),
                hi,
                |state| {
                    let HostControlState::Pending(mut pending) = state else {
                        panic!("no control approval was supplied");
                    };
                    assert!(pending.request().is_none());
                    assert!(pending.native_status().is_none());
                    assert!(!seat.is_occupied());
                    Ok(pending.configuration()?.map(target))
                },
                || super::super::super::tests::nonce(&mut entropy),
                || None,
                super::super::super::tests::block,
            ),
            async {
                let result = viewer
                    .serve_interactive_control(
                        vi,
                        request,
                        InputPolicy::default(),
                        |state, _| {
                            let InteractiveViewerState::Viewing(view) = state else {
                                panic!("a failed chain must not progress the pending request");
                            };
                            callback = true;
                            assert_eq!(view.request(), request);
                            match case {
                                Terminal::PendingRequest => {
                                    assert!(!terminal);
                                    view.request_control().unwrap();
                                    terminal = true;
                                }
                                Terminal::LocalStop if view.presentation().is_none() => {
                                    assert!(view.request_control().is_err());
                                    terminal = true;
                                    view.stop();
                                }
                                Terminal::LocalStop => {}
                            }
                            Ok(())
                        },
                        |_| panic!("no input owner was granted"),
                        super::super::super::tests::block,
                    )
                    .await;
                stop.revoke();
                result
            },
        ))
        .await;
        assert!(
            callback && terminal,
            "host={host_result:?} viewer={viewer_result:?}"
        );
        if matches!(case, Terminal::PendingRequest) {
            assert_eq!(
                viewer_result,
                Err(crate::session_startup::StreamingViewerError::Delivery(
                    fr_media::delivery::DeliveryError::ReferenceExpired,
                ))
            );
        } else {
            assert!(viewer_result.is_err());
        }
        assert!(host_result.is_err());
        assert_eq!(viewer.statistics().recovered_streams, 0);
        assert_eq!(host.statistics().recovered_streams, 0);
        assert!(!seat.is_occupied());
        assert!(stop.check().is_err());
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
