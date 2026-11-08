//! Early native adapter errors fence the original, fully bootstrapped observer.
//! TLS, worker ownership and process reaping are real; ingress, HEVC completion
//! and local platform permissions use the enclosing explicit protocol fixtures.
use super::*;
use frd::native_connection::reconnect::{
    Application, CallbackError, Status, native_control_view_with_setup, native_view,
};

async fn failed_application(
    application: &mut impl Application<Output = ()>,
    viewer: Viewer,
    cleanup: &Cx,
    retirement: &mut Retirement,
) {
    let original = viewer.control();
    assert!(!original.is_stopped());
    let result = Box::pin(application.run(1, viewer)).await;
    assert_eq!(result, Err(ObserverError::Application));
    assert!(
        original.is_stopped(),
        "the original observer must close before its callback error escapes"
    );
    assert_eq!(application.terminal_failure(), None);
    assert_eq!(
        application.status(Status::Connecting { attempt: 2 }),
        Err(CallbackError),
        "a fence does not authorize retry before native cleanup"
    );
    let deadline = Deadline::after(cleanup, Duration::from_secs(1)).unwrap();
    assert_eq!(
        retirement.reap(cleanup, deadline).await,
        Err(frd::worker::Error::ReapPending),
        "the adapter retains its original child until explicit cleanup"
    );
    application.cleanup(cleanup, deadline).await.unwrap();
    assert!(
        retirement.reap(cleanup, deadline).await.unwrap().is_some(),
        "cleanup must produce the original child's actual OS exit receipt"
    );
    assert!(original.is_stopped());
    assert_eq!(application.terminal_failure(), None);
    application
        .status(Status::Connecting { attempt: 2 })
        .unwrap();
    // A second cleanup neither manufactures another worker nor loses the
    // already-confirmed exit of the original native owner.
    application.cleanup(cleanup, deadline).await.unwrap();
    assert!(retirement.reap(cleanup, deadline).await.unwrap().is_some());
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress and native-effect fixtures"]
fn initial_observation_ui_failure_fences_before_return_and_retains_cleanup() {
    callback_failure(false);
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress and native-effect fixtures"]
fn control_setup_failure_fences_before_return_and_retains_cleanup() {
    callback_failure(true);
}

#[allow(clippy::too_many_lines)]
fn callback_failure(controlled: bool) {
    run(async |broker, supervisor, client, runtime| {
        let api = fixture::Api::new();
        let tools = Tools::new();
        let mut server = bound(&broker, &api, &tools).await;
        let source = runtime
            .try_request_cx_with_budget(Budget::INFINITE)
            .unwrap();
        let (setup, _, mut source_retirement, _) = setup(source.clone(), "normal");
        let (image, _) = input_fixture();
        let mut driver = control_driver(source, &image, Seat::default());
        let connections = Connections {
            request: unattended_request,
            approval: |_, _| panic!("explicit unattended profile"),
            completed: |_, _| Ok(serial::Action::Continue),
        };
        let serving = server.serve_desktop(
            &mut driver,
            supervisor.clone(),
            runtime,
            serial::Policy::default(),
            connections,
            move || async move { Ok(setup) },
            choose,
            |_, _| Ok(LocalAction::Continue),
        );
        let viewing = async {
            let offer = if controlled {
                fr_client::native::control_offer()
            } else {
                fr_client::native::observation_offer()
            };
            let viewer = fixture_viewer(&client, offer).await;
            let original = viewer.control();
            let (launch, mut retirement) = presenter_fixture().retain_cleanup().unwrap();
            let mut launch = Some(launch);
            let calls = Cell::new(0_u8);
            if controlled {
                let mut application = native_control_view_with_setup(
                    ObserverPolicy::default(),
                    ClockPolicy::default(),
                    InputPolicy::default(),
                    1,
                    keys(),
                    |_| launch.take().ok_or(CallbackError),
                    |_, catalog| Ok(Some(catalog.displays()[0].handle)),
                    |_, _| panic!("explicit unattended profile"),
                    |_, _, _| panic!("failed setup must not enter interactive service"),
                    |_, _| panic!("setup failure creates no input receipt"),
                    |_| Ok(()),
                    |attempt, observer| {
                        assert_eq!(attempt, 1);
                        assert_eq!(calls.replace(1), 0);
                        assert!(observer.worker_id().is_some());
                        assert!(!observer.control().is_stopped());
                        assert!(!original.is_stopped());
                        Err(CallbackError)
                    },
                );
                failed_application(&mut application, viewer, &broker, &mut retirement).await;
            } else {
                let mut application = native_view(
                    ObserverPolicy::default(),
                    |_| launch.take().ok_or(CallbackError),
                    |_, catalog| Ok(Some(catalog.displays()[0].handle)),
                    |_, _| panic!("explicit unattended profile"),
                    |attempt, initial| {
                        assert_eq!(attempt, 1);
                        assert_eq!(calls.replace(1), 0);
                        assert!(initial.is_some());
                        assert!(!original.is_stopped());
                        Err(CallbackError)
                    },
                    |_| Ok(()),
                );
                failed_application(&mut application, viewer, &broker, &mut retirement).await;
            }
            assert_eq!(calls.get(), 1);
            assert!(launch.is_none());
            supervisor.cancel_fast(CancelKind::User);
        };
        let (end, ()) = Box::pin(network::both(serving, viewing)).await;
        assert_eq!(end, End::Cancelled);
        cleanup(
            &mut server,
            &mut driver,
            &mut source_retirement,
            &broker,
            true,
        )
        .await;
    });
}
