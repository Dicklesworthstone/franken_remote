//! Real approved bootstrap on fresh connections; setup/visibility remain local fixtures.
use super::*;
use crate::native_connection::reconnect::{
    Application, CallbackError, native_control_view_with_setup,
};
use std::cell::RefCell;

#[test]
fn native_application_reconnect_setup_is_fresh_and_failed_setup_retains_cleanup_owner() {
    let runtime = support::runtime();
    runtime.block_on(async {
        let handles = RefCell::new(Vec::new());
        let attempts = RefCell::new(Vec::new());
        let os = Arc::new(Mutex::new(Os::default()));
        let cleanup = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
        let mut app = native_control_view_with_setup(
            ObserverPolicy::default(),
            ClockPolicy::default(),
            fr_client::input::Policy::default(),
            1,
            Capabilities::default().with(Capability::Keys),
            |_| Ok(launch(WorkerRole::Present)),
            |_, cat| Ok(Some(cat.displays()[0].handle)),
            |_, _| Ok(()),
            |_, _, _| panic!("failed setup cannot start interactive UI"),
            |_, _| panic!("failed setup cannot submit input"),
            |_| Ok(()),
            |attempt, observer| {
                attempts.borrow_mut().push(attempt);
                let control = observer
                    .configure_clipboard(configured(&os, true, None))
                    .unwrap();
                assert_eq!(control.status().phase, Phase::WaitingForControl);
                handles.borrow_mut().push(control);
                Err(CallbackError)
            },
        );
        for attempt in 1..=2 {
            let client = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
            let host_cx = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
            let (host, viewer) = crate::session_startup::running::tests::pair_before_finish(
                &client,
                &host_cx,
                caps(),
            )
            .await;
            let host = host.finish().unwrap().into_running().unwrap();
            let mut nonce = 90_000;
            let (host, result) = Box::pin(support::both(
                host.publish_controlled_display(
                    launch(WorkerRole::Capture),
                    PublisherPolicy::default(),
                    config,
                    || {
                        nonce += 1;
                        Ok(nonce)
                    },
                ),
                app.run(attempt, viewer),
            ))
            .await;
            let mut host = host.unwrap();
            assert!(matches!(
                result,
                Err(session_startup::ObserverError::Application)
            ));
            assert!(!os.lock().unwrap().opened);
            let deadline = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
            app.cleanup(&cleanup, deadline).await.unwrap();
            host.reap_media(&cleanup, deadline).await.unwrap();
            for control in handles.borrow().iter() {
                assert_eq!(control.status().phase, Phase::Closed);
                assert_eq!(control.status().cleanup, Cleanup::NotStarted);
                assert_eq!(control.set_enabled(true), Err(ClipboardError::Closed));
            }
        }
        assert_eq!(*attempts.borrow(), [1, 2]);
        assert_eq!(handles.borrow().len(), 2);
        assert!(!os.lock().unwrap().opened);
    });
}
