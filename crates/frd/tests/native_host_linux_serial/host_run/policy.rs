//! Installed host integration, not a live-tailnet/OS capture qualification.
//! Uses the existing real TLS/UDP owner and scripted source/LocalAPI/ingress.
use super::*;
use frd::host_policy::{Approval, Change, Sharing, Store, live};
use frd::host_run::policy::Configuration;
use std::time::Instant;

fn configuration(tools: &Tools) -> Configuration {
    let path = tools.0.join("host-policy.json");
    Store::new(&path)
        .unwrap()
        .update(Change::Approval(Approval::None))
        .unwrap();
    Configuration {
        path,
        approval: None,
        sharing: None,
    }
}
fn counts(events: &Mutex<Vec<Event>>, test: fn(&Event) -> bool) -> usize {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| test(event))
        .count()
}
fn listening(event: &Event) -> bool {
    matches!(event, Event::Listening { .. })
}
fn finished(event: &Event) -> bool {
    matches!(event, Event::PeerFinished { .. })
}
fn clean(events: &Mutex<Vec<Event>>, tools: &Tools, shares: u64) {
    let events = events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::CleanupFailed { .. })),
        "{events:?}"
    );
    assert!(matches!(events.last(), Some(Event::Stopped)), "{events:?}");
    assert_eq!(tools.state()["created"], shares);
    assert_eq!(tools.state()["deleted"], shares);
    assert!(
        UdpSocket::bind(address()).is_ok(),
        "original transport retired"
    );
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn live_local_approval_refuses_before_certificates_listeners_or_capture() {
    let api = fixture::Api::new();
    let tools = Tools::new();
    let (worker, trace) = super::super::persistent_desktop::source_script("normal");
    let cfg = configuration(&tools);
    Store::new(&cfg.path)
        .unwrap()
        .update(Change::Approval(Approval::Local))
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    let result = host_run::run_with_policy(
        &options(&api, &tools, worker),
        &report,
        &Arc::new(StopHandle::default()),
        cfg,
    );
    assert_eq!(result, Err(host_run::Error::LocalApprovalUnavailable));
    assert_eq!(
        api.calls.load(Ordering::SeqCst),
        0,
        "no credential or identity I/O"
    );
    assert!(!trace.exists());
    assert_eq!(*events.lock().unwrap(), vec![Event::Stopped]);
}

#[derive(Clone, Copy)]
enum Update {
    Approval,
    Corrupt,
    Overridden,
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn live_approval_change_retires_active_share_even_when_viewer_stops_answering() {
    update_active(Update::Approval);
}
#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn malformed_live_policy_stops_active_capture_without_an_unsafe_restart() {
    update_active(Update::Corrupt);
}
#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn overridden_disk_revision_still_retires_old_source_before_reopening() {
    update_active(Update::Overridden);
}
fn update_active(update: Update) {
    let api = fixture::Api::new();
    let tools = Tools::new();
    let (worker, trace) = super::super::persistent_desktop::source_script("normal");
    let mut cfg = configuration(&tools);
    if matches!(update, Update::Overridden) {
        cfg.approval = Some(Approval::None);
    }
    let path = cfg.path.clone();
    let mut options = options(&api, &tools, worker);
    options.once = false;
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    let stop = Arc::new(StopHandle::default());
    let host_stop = stop.clone();
    let host =
        std::thread::spawn(move || host_run::run_with_policy(&options, &report, &host_stop, cfg));
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let action = runtime.block_on(async {
        timeout(cx.now(), Duration::from_secs(15), async {
            until(&cx, || {
                counts(&events, listening) == 1 || host.is_finished()
            })
            .await;
            assert!(!host.is_finished(), "{:?}", events.lock().unwrap());
            assert!(!trace.exists(), "no source before admission");
            let native = fixture::client(&cx, address()).await;
            let viewer = Viewer::new(
                cx.clone(),
                native,
                super::super::persistent_desktop::offer(),
                fr_transport::quic::Policy::default(),
                Duration::from_secs(3),
            )
            .unwrap();
            let mut viewer = Box::pin(Client::start(cx.clone(), viewer)).await;
            viewer.ready().await;
            assert_eq!(viewer.frames.first(), Some(&0));
            let pid = fs::read_to_string(&trace).unwrap();
            let started = Instant::now();
            match update {
                Update::Corrupt => fs::write(&path, b"{invalid policy").unwrap(),
                Update::Approval | Update::Overridden => {
                    Store::new(&path)
                        .unwrap()
                        .update(Change::Approval(Approval::Local))
                        .unwrap();
                }
            }
            // Deliberately do not drive/renew the viewer. The local policy fence
            // must not wait for its remote acknowledgement or silence deadline.
            until(&cx, || {
                host.is_finished() || counts(&events, listening) == 2
            })
            .await;
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "must beat viewer's three-second lease"
            );
            assert!(
                !PathBuf::from(format!("/proc/{pid}")).exists(),
                "old worker was reaped"
            );
            if matches!(update, Update::Overridden) {
                assert!(!host.is_finished());
                assert_eq!(counts(&events, listening), 2);
                assert_eq!(
                    fs::read_to_string(&trace).unwrap(),
                    pid,
                    "new source remains lazy"
                );
                assert_eq!(
                    Store::new(&path).unwrap().load().unwrap().approval_mode,
                    Approval::Local,
                    "process override never writes saved policy"
                );
            } else {
                assert!(
                    host.is_finished(),
                    "no rebind over local approval or invalid evidence"
                );
            }
            // Retain the original viewer until after the host's terminal effect.
            drop(viewer);
        })
        .await
    });
    stop.request();
    let result = host.join().unwrap();
    action.unwrap();
    assert_result(update, &result);
    clean(
        &events,
        &tools,
        if matches!(update, Update::Overridden) {
            2
        } else {
            1
        },
    );
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn live_saved_scope_overrides_permissive_startup_options_before_admission() {
    let api = fixture::Api::new();
    *api.mode.lock().unwrap() = fixture::Mode::OtherUser;
    let tools = Tools::new();
    let (worker, trace) = super::super::persistent_desktop::source_script("normal");
    let cfg = configuration(&tools);
    assert_eq!(
        Store::new(&cfg.path).unwrap().load().unwrap().sharing_scope,
        Sharing::OwnUser
    );
    let mut options = options(&api, &tools, worker);
    options.sharing = fr_tailnet::Scope::Tailnet;
    options.once = false;
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    let stop = Arc::new(StopHandle::default());
    let host_stop = stop.clone();
    let host =
        std::thread::spawn(move || host_run::run_with_policy(&options, &report, &host_stop, cfg));
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let action = runtime.block_on(async {
        timeout(cx.now(), Duration::from_secs(10), async {
            until(&cx, || counts(&events, listening) == 1).await;
            let native = fixture::client(&cx, address()).await;
            until(&cx, || counts(&events, finished) == 1).await;
            assert!(!trace.exists(), "refused peer cannot create capture");
            assert!(events.lock().unwrap().iter().any(
                |event| matches!(event, Event::PeerFinished { admitted: 0, refused: 1, outcome, .. }
                    if outcome.contains("ScopeDenied"))
            ));
            drop(native);
        })
        .await
    });
    stop.request();
    let result = host.join().unwrap();
    action.unwrap();
    assert_eq!(result, Ok(()));
    clean(&events, &tools, 1);
}

fn assert_result(update: Update, result: &Result<(), host_run::Error>) {
    match update {
        Update::Approval => assert_eq!(*result, Err(host_run::Error::LocalApprovalUnavailable)),
        Update::Corrupt => assert_eq!(
            *result,
            Err(host_run::Error::Policy(live::Error::Store(
                host_policy::Error::InvalidDocument
            )))
        ),
        Update::Overridden => assert_eq!(*result, Ok(())),
    }
}
