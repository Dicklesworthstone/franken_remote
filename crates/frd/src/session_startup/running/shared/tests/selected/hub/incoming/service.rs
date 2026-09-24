//! Scoped incoming ownership against real TLS/UDP and the canonical hub.
//! Source pictures, permission and decode acknowledgements remain fixtures.
use super::*;
use std::task::{Context, Waker};

#[test]
fn unpolled_scoped_host_cancels_only_its_original_viewer() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, _first, mut hub, admission, initial) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let Fresh {
            host,
            h,
            viewer: _viewer,
            ..
        } = *Box::pin(fresh(&rt, 14, true, Role::Observe, Duration::from_secs(2))).await;
        let run = admission
            .serve_host(host, |_, _| panic!("unpolled notification"))
            .unwrap();
        let ticket = run.ticket();
        assert_eq!(ticket.state(), State::Opening);
        assert!(h.checkpoint().is_ok());
        drop(run);
        assert_eq!(ticket.state(), State::Finished(Err(service::Error::Closed)));
        assert!(h.checkpoint().is_err());
        assert_eq!(initial.state(), State::Serving);
        assert!(owner.check().is_ok());
        hub.close();
        reap(&mut publisher).await;
    });
}

#[test]
fn scoped_host_survives_handoff_approval_and_streaming_until_its_ticket_closes() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, mut first, mut hub, admission, initial) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let pid = publisher.worker_id();
        let Fresh {
            c,
            h,
            host,
            mut viewer,
            ..
        } = *Box::pin(fresh(&rt, 14, true, Role::Observe, Duration::from_secs(3))).await;
        let decision = Arc::new(Mutex::new(None));
        let copy = decision.clone();
        let mut run = pin!(
            admission
                .serve_host(host, move |a, _| {
                    *copy.lock().unwrap() = Some(a);
                    Ok(())
                })
                .unwrap()
        );
        let ticket = run.ticket();
        let result = Box::pin(together(
            &mut hub,
            &mut publisher,
            with_client(&mut first.peer, async {
                let client = async {
                    prompt(&mut viewer, &decision).await;
                    assert_eq!(ticket.state(), State::Opening);
                    assert!(h.checkpoint().is_ok());
                    decision
                        .lock()
                        .unwrap()
                        .take()
                        .unwrap()
                        .decide(true)
                        .unwrap();
                    let mut client = Box::pin(Client::start(c, viewer)).await;
                    client.ready().await;
                    assert_eq!(ticket.state(), State::Serving);
                    assert!(h.checkpoint().is_ok());
                    assert_ne!(client.frames.len(), 0);
                    assert_eq!(initial.state(), State::Serving);
                    ticket.close();
                    client.viewer.close();
                };
                let (outcome, ()) = Box::pin(support::both(run.as_mut(), client)).await;
                assert!(owner.check().is_ok());
                assert_eq!(initial.state(), State::Serving);
                outcome
            }),
        ))
        .await;
        assert_eq!(result, Err(service::Error::Closed));
        // A retained, completed future is terminal and harmless to siblings.
        let mut context = Context::from_waker(Waker::noop());
        assert_eq!(run.as_mut().poll(&mut context), Poll::Ready(result));
        assert_eq!(ticket.state(), State::Finished(result));
        assert_eq!(publisher.worker_id(), pid);
        hub.close();
        reap(&mut publisher).await;
    });
}

#[test]
fn scoped_approval_failure_preserves_hub_result_without_waiting_for_timer_expiry() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, mut first, mut hub, admission, initial) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let Fresh {
            host,
            h,
            mut viewer,
            ..
        } = *Box::pin(fresh(&rt, 14, true, Role::Observe, Duration::from_secs(3))).await;
        let mut run = pin!(admission.serve_host(host, |_, _| Err(())).unwrap());
        let ticket = run.ticket();
        let result = Box::pin(together(
            &mut hub,
            &mut publisher,
            with_client(&mut first.peer, async {
                let client = async {
                    finished(&mut viewer, &ticket).await;
                };
                let result = Box::pin(support::both(run.as_mut(), client)).await.0;
                assert!(owner.check().is_ok());
                assert_eq!(initial.state(), State::Serving);
                result
            }),
        ))
        .await;
        assert!(result.is_err());
        assert_eq!(ticket.state(), State::Finished(result));
        assert!(h.checkpoint().is_err());
        hub.close();
        reap(&mut publisher).await;
    });
}

#[test]
fn dropping_parked_scoped_host_fences_after_notification_even_with_retained_ticket() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, mut first, mut hub, admission, initial) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let Fresh {
            host,
            h,
            mut viewer,
            ..
        } = *Box::pin(fresh(&rt, 14, true, Role::Observe, Duration::from_secs(3))).await;
        let local = Arc::new(Mutex::new(None));
        let copy = local.clone();
        let mut run = Box::pin(
            admission
                .serve_host(host, move |a, _| {
                    *copy.lock().unwrap() = Some(a);
                    Ok(())
                })
                .unwrap(),
        );
        let ticket = run.ticket();
        Box::pin(together(
            &mut hub,
            &mut publisher,
            with_client(&mut first.peer, async {
                let mut prompting = pin!(prompt(&mut viewer, &local));
                poll_fn(|task| {
                    assert!(run.as_mut().poll(task).is_pending());
                    prompting.as_mut().poll(task)
                })
                .await;
                drop(run);
                assert!(h.checkpoint().is_err());
                assert_eq!(ticket.state(), State::Finished(Err(service::Error::Closed)));
                assert!(local.lock().unwrap().take().unwrap().decide(true).is_err());
                assert!(owner.check().is_ok());
                assert_eq!(initial.state(), State::Serving);
            }),
        ))
        .await;
        hub.close();
        reap(&mut publisher).await;
    });
}

#[test]
fn hub_shutdown_completes_scoped_host_without_reviving_a_finished_receipt() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, _owner, _first, mut hub, admission, _) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let Fresh {
            host,
            h,
            viewer: _viewer,
            ..
        } = *Box::pin(fresh(&rt, 14, false, Role::Observe, Duration::from_secs(2))).await;
        let mut run = Box::pin(admission.serve_host(host, |_, _| Ok(())).unwrap());
        let ticket = run.ticket();
        let mut context = Context::from_waker(Waker::noop());
        assert!(run.as_mut().poll(&mut context).is_pending());
        hub.close();
        assert_eq!(run.as_mut().await, Err(service::Error::Closed));
        assert_eq!(ticket.state(), State::Finished(Err(service::Error::Closed)));
        assert!(h.checkpoint().is_err());
        drop(run);
        reap(&mut publisher).await;
    });
}

#[test]
fn parked_service_keeps_the_original_startup_deadline_without_polling_the_hub() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, _first, mut hub, admission, initial) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let Fresh {
            host,
            h,
            viewer: _viewer,
            ..
        } = *Box::pin(fresh(
            &rt,
            14,
            true,
            Role::Observe,
            Duration::from_millis(40),
        ))
        .await;
        let run = admission
            .serve_host(host, |_, _| panic!("expired host cannot notify"))
            .unwrap();
        let ticket = run.ticket();
        let result = run.await;
        assert_eq!(result, Err(service::Error::Session(OpenError::Expired)));
        assert_eq!(ticket.state(), State::Finished(result));
        assert!(h.checkpoint().is_err());
        assert!(owner.check().is_ok());
        assert_eq!(initial.state(), State::Serving);
        hub.close();
        reap(&mut publisher).await;
    });
}

#[cfg(target_os = "linux")]
#[path = "native_admission.rs"]
mod native;

#[path = "native_source.rs"]
mod native_source;
