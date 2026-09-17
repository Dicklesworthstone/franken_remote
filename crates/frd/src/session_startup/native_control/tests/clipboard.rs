//! Public native bootstrap + real UDP/TLS + supervised media children and
//! clipboard threads. Discovery/codec/OS effects/visibility are explicit fixtures.
use super::*;
use crate::clipboard_quic::Error as ClipboardError;
use crate::native_clipboard::{Cleanup, Configuration as ClipboardConfiguration, Phase};
use crate::session_startup::ManagedHostControlState;
use fr_core::clipboard::{ClipboardSink, Publication, Stamp};
use fr_wire::clipboard::session::synchronize::{
    NativeChange, NativeChanges, NativeClipboard, NativeText, Received,
};

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // Independent fixture lifecycle and blocking controls.
struct Os {
    revision: u64,
    text: Option<String>,
    origin: Option<Stamp>,
    published: Vec<String>,
    opened: bool,
    closed: bool,
    block: bool,
    entered: bool,
    release: bool,
}
struct Release(Arc<Mutex<Os>>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.lock().unwrap().release = true;
    }
}

fn copy(os: &Arc<Mutex<Os>>, text: &str) {
    let mut os = os.lock().unwrap();
    os.revision += 1;
    os.text = Some(text.into());
    os.origin = None;
}
struct Text(String, Option<Stamp>);
impl NativeText for Text {
    fn text(&self) -> &str {
        &self.0
    }
    fn origin(&self) -> Option<Stamp> {
        self.1
    }
}
struct Native {
    os: Arc<Mutex<Os>>,
    seen: u64,
    reading: bool,
}
impl ClipboardSink for Native {
    fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), fr_core::clipboard::PlatformError> {
        Ok(())
    }
    fn publish(&mut self, text: &str, stamp: Stamp) -> Publication {
        let mut os = self.os.lock().unwrap();
        os.text = Some(text.into());
        os.origin = Some(stamp);
        os.revision += 1;
        os.published.push(text.into());
        self.reading = false;
        Publication::SubmittedToOs
    }
}
impl NativeClipboard for Native {
    type Text = Text;
    type Error = ();
    fn watch(&mut self) -> Result<u64, ()> {
        Ok(self.seen)
    }
    fn revision(&self) -> u64 {
        self.os.lock().unwrap().revision
    }
    fn changes(&mut self) -> Result<NativeChanges, ()> {
        let os = self.os.lock().unwrap();
        let latest = (os.revision != self.seen).then_some(NativeChange {
            revision: os.revision,
            has_selection: os.text.is_some(),
            origin: os.origin,
        });
        self.seen = os.revision;
        Ok(NativeChanges {
            latest,
            settled: true,
        })
    }
    fn prepare_for_revision(
        &mut self,
        _: &str,
        _: Stamp,
        revision: u64,
    ) -> Result<(), fr_core::clipboard::PlatformError> {
        if self.revision() == revision {
            Ok(())
        } else {
            Err(fr_core::clipboard::PlatformError::Unavailable)
        }
    }
    fn begin_read(&mut self) -> Result<(), ()> {
        self.reading = true;
        Ok(())
    }
    fn poll_read(&mut self) -> Result<Option<Text>, ()> {
        if !self.reading {
            return Ok(None);
        }
        self.reading = false;
        let os = self.os.lock().unwrap();
        Ok(os.text.as_ref().map(|s| Text(s.clone(), os.origin)))
    }
    fn cancel_read(&mut self) {
        self.reading = false;
    }
    fn suspend(&mut self) -> Result<(), ()> {
        self.reading = false;
        Ok(())
    }
    fn close(&mut self) {
        self.os.lock().unwrap().closed = true;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Transfer,
    RetainReceipts,
    DeclineHost,
    DeclineViewer,
    StopBeforeControl(bool),
    DisableBeforeControl(bool),
    FactoryFailure,
    FactoryPanic,
    BlockedFactory,
}
fn caps() -> Vec<WireCapability> {
    let mut caps = capabilities();
    for name in [
        attachment::CLIPBOARD_CAPABILITY,
        fr_wire::clipboard::CAPABILITY,
        fr_wire::clipboard::startup::CAPABILITY,
    ] {
        caps.push(WireCapability {
            name: name.into(),
            version: 1,
            required: true,
        });
    }
    caps.sort_by(|a, b| a.name.cmp(&b.name));
    caps
}
fn configured(os: &Arc<Mutex<Os>>, consent: bool, failure: Option<bool>) -> ClipboardConfiguration {
    let os = os.clone();
    let mut id = 0u128;
    ClipboardConfiguration::new(
        consent,
        Duration::from_secs(2),
        move || {
            if let Some(panic) = failure {
                assert!(!panic, "explicit fixture native initialization panic");
                return Err(fr_core::clipboard::PlatformError::Unavailable);
            }
            // Explicit hung-driver fixture; the test owns an unwind-safe release.
            os.lock().unwrap().entered = true;
            while {
                let os = os.lock().unwrap();
                os.block && !os.release
            } {
                std::thread::sleep(Duration::from_millis(1));
            }
            os.lock().unwrap().opened = true;
            Ok(Native {
                os,
                seen: 0,
                reading: false,
            })
        },
        move || {
            id += 1;
            Ok(id)
        },
    )
    .unwrap()
}
#[allow(clippy::too_many_lines)]
async fn exercise(c: Cx, h: Cx, cleanup: Cx, scenario: Scenario) {
    let (mut host, viewer) = pair_initialized(&c, &h, caps(), |_| {}).await;
    let authority = host.observation().unwrap();
    let mut n = 60_000u128;
    let (host, viewer) = Box::pin(support::both(
        host.publish_controlled_display(
            launch(WorkerRole::Capture),
            PublisherPolicy::default(),
            config,
            || {
                n += 1;
                Ok(u128::MAX - n) // Unordered public bindings must not depend on entropy.
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
    let mut host = host.unwrap();
    let mut viewer = viewer.unwrap();
    let original = (host.worker_id(), viewer.worker_id());
    let os = [
        Arc::new(Mutex::new(Os::default())),
        Arc::new(Mutex::new(Os::default())),
    ];
    let release = Release(os[0].clone());
    os[0].lock().unwrap().block = scenario == Scenario::BlockedFactory;
    let local = host
        .configure_clipboard(configured(
            &os[0],
            scenario != Scenario::DeclineHost,
            match scenario {
                Scenario::FactoryFailure => Some(false),
                Scenario::FactoryPanic => Some(true),
                _ => None,
            },
        ))
        .unwrap();
    let remote = viewer
        .configure_clipboard(configured(
            &os[1],
            scenario != Scenario::DeclineViewer,
            None,
        ))
        .unwrap();
    assert_eq!(local.status().phase, Phase::WaitingForControl);
    assert!(local.status().enabled && remote.status().enabled);
    match scenario {
        Scenario::StopBeforeControl(host_side) => {
            let control = if host_side { &local } else { &remote };
            control.stop();
            assert_eq!(control.set_enabled(true), Err(ClipboardError::Closed));
        }
        Scenario::DisableBeforeControl(host_side) => {
            let control = if host_side { &local } else { &remote };
            control.set_enabled(false).unwrap();
        }
        _ => {}
    }
    // Capture is already running, but no clipboard may open before control.
    assert!(!os[0].lock().unwrap().opened && !os[1].lock().unwrap().opened);
    copy(&os[0], "host copy");
    let keys = Capabilities::default().with(Capability::Keys);
    let target = host.control_target(keys).unwrap();
    let request = viewer.control_request(7, keys).unwrap();
    let stop = viewer.control();
    let seat = Seat::default();
    let effects = Arc::new(Mutex::new(Vec::new()));
    let mut ticket = 70_000u128;
    let receipts = Cell::new(0);
    let start = now(&c).unwrap();
    let mut shown = None;
    let mut requested = false;
    let mut actions = 0;
    let mut copies = 0;
    let mut first_publication = None;
    let mut retired = false;
    let mut complete = false;
    let (hr,vr)=Box::pin(support::both(
        async {
            let result=host.serve_managed_control(seat.clone(),|state| {
                if let ManagedHostControlState::Pending(mut pending)=state
                    && pending.request().is_some() && pending.native_status().is_none() && pending.view_ready()? {
                    let effects=effects.clone();
                    pending.approve(target,||Some((InputLeaseId::from_raw(91),InputTicketId::from_raw(92))),move||Ok(Sink(effects)),|_|true)?;
                }
                Ok(Some(target))
            },||{n+=1;Ok(u128::MAX-n)},||{ticket+=1;Some(InputTicketId::from_raw(ticket))}).await;
            stop.stop(); result
        },
        async {
            let result=viewer.serve_interactive_control(7,keys,fr_client::input::Policy::default(),|state,frame| {
                assert!(now(&c).unwrap()<start+4_000_000, "native application stalled: {:?} {:?}",local.status(),remote.status());
                match state {
                    session_startup::InteractiveViewerState::Viewing(viewing)=> {
                        assert!(!os[0].lock().unwrap().opened && !os[1].lock().unwrap().opened);
                        viewing.confirm_mapping(request.parent, target.view).unwrap();
                        if let Some(p)=viewing.presentation() && shown!=Some(p.frame) {
                            viewing.visible(p.frame.as_raw()).unwrap(); shown=Some(p.frame);
                        }
                        if !requested && now(&c).unwrap()>start+100_000 {
                            viewing.request_control().unwrap(); requested=true;
                        }
                    }
                    session_startup::InteractiveViewerState::Requesting(_)=> {},
                    session_startup::InteractiveViewerState::Controlled(input)=> {
                        if let Some(p)=frame { input.visible(p.frame.as_raw()).unwrap(); }
                        if scenario==Scenario::Transfer {
                            let receipt=if copies==1 {local.take_received()}else{remote.take_received()};
                            if let Some(receipt)=receipt {
                                assert!(matches!(receipt,Received::Consumed(Some(r)) if r.publication==Publication::SubmittedToOs));
                                match copies {
                                    0=>{assert_eq!(os[1].lock().unwrap().text.as_deref(),Some("host copy"));copy(&os[1],"viewer λ 👋");},
                                    1=>{assert_eq!(os[0].lock().unwrap().text.as_deref(),Some("viewer λ 👋"));copy(&os[0],"");},
                                    2=>assert_eq!(os[1].lock().unwrap().text.as_deref(),Some("")),
                                    _=>panic!("unexpected echo"),
                                }
                                copies+=1;
                            }
                        }
                        if scenario == Scenario::RetainReceipts {
                            let published = os[1].lock().unwrap().published.len();
                            if copies == 0 && published == 1 {
                                // This tests unread receipts, not superseding an in-flight
                                // native send. Allow the actual peer ACK/drain turns first.
                                let first = *first_publication.get_or_insert(now(&c).unwrap());
                                if now(&c).unwrap() >= first + 100_000 { copy(&os[0], "final copy"); copies = 1; }
                            }
                            if copies == 1 && published == 2 { copies = 2; }
                        }
                        let ready=if scenario==Scenario::Transfer { copies==3 } else if scenario==Scenario::RetainReceipts { copies==2 } else if scenario==Scenario::BlockedFactory { os[0].lock().unwrap().entered } else {
                            matches!(local.status().phase,Phase::Retired) && matches!(remote.status().phase,Phase::Retired)
                        };
                        if ready && actions==0 {
                            let _=input.action(fr_client::input::Action::Key{key:PhysicalKey::new(4).unwrap(),transition:KeyTransition::Press}).unwrap();actions=1;
                        }
                        if actions==1 && receipts.get()==1 {
                            if !matches!(scenario, Scenario::BlockedFactory | Scenario::RetainReceipts) { local.stop();retired=true; }
                            if matches!(scenario, Scenario::BlockedFactory | Scenario::RetainReceipts) || input.clipboard_retired() {
                                let _=input.action(fr_client::input::Action::Key{key:PhysicalKey::new(4).unwrap(),transition:KeyTransition::Release}).unwrap();actions=2;
                            }
                        }
                        if actions==2 && receipts.get()==2 { complete=true;retired=true; stop.stop(); authority.revoke(); }
                    }
                }
                Ok(())
            },|_|receipts.set(receipts.get()+1)).await;
            authority.revoke();result
        },
    )).await;
    assert!(hr.session.is_err() && vr.is_err(), "{hr:?} {vr:?}");
    assert!(requested);
    let failed_factory = matches!(scenario, Scenario::FactoryFailure | Scenario::FactoryPanic);
    // Foreign worker failure can race an active QUIC future. Its retained-byte
    // authorization guard then closes the original session conservatively;
    // an application must not promise clipboard-only isolation for this case.
    if !failed_factory {
        assert!(complete && retired);
    }
    assert!(hr.input.unwrap().handoff_safe());
    assert!(!seat.is_occupied());
    if !failed_factory {
        assert_eq!(effects.lock().unwrap().len(), 2);
    }
    assert_eq!((host.worker_id(), viewer.worker_id()), original);
    assert_eq!(local.status().phase, Phase::Closed);
    assert_eq!(remote.status().phase, Phase::Closed);
    assert_eq!(local.set_enabled(true), Err(ClipboardError::Closed));
    if scenario == Scenario::BlockedFactory {
        {
            let os = os[0].lock().unwrap();
            assert!(os.entered && !os.opened);
        }
        assert_eq!(local.status().cleanup, Cleanup::Pending);
        let deadline = Deadline::after(&cleanup, Duration::from_millis(5)).unwrap();
        assert_eq!(
            host.reap_clipboard(&cleanup, deadline).await,
            Err(ClipboardError::HandoffExpired)
        );
        assert_eq!(
            local.status().cleanup,
            Cleanup::Pending,
            "timeout must retain unjoined worker"
        );
        assert!(local.take_received().is_none());
    }
    drop(release);
    let deadline = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
    let host_cleanup = host.reap_clipboard(&cleanup, deadline).await.unwrap();
    let viewer_cleanup = viewer.reap_clipboard(&cleanup, deadline).await.unwrap();
    match scenario {
        Scenario::RetainReceipts => {
            assert_eq!(copies, 2);
            assert!(matches!(viewer_cleanup, Cleanup::Finished(_)));
            assert_eq!(os[1].lock().unwrap().published.len(), 2);
        }
        Scenario::Transfer => {
            assert_eq!(copies, 3);
            assert!(
                matches!(host_cleanup, Cleanup::Finished(_))
                    && matches!(viewer_cleanup, Cleanup::Finished(_))
            );
            assert_eq!(os[0].lock().unwrap().published.len(), 1);
            assert_eq!(os[1].lock().unwrap().published.len(), 2);
            assert!(os[0].lock().unwrap().closed && os[1].lock().unwrap().closed);
        }
        Scenario::DeclineHost
        | Scenario::DeclineViewer
        | Scenario::StopBeforeControl(_)
        | Scenario::DisableBeforeControl(_) => {
            assert_eq!(
                (host_cleanup, viewer_cleanup),
                (Cleanup::NotStarted, Cleanup::NotStarted)
            );
            assert!(!os[0].lock().unwrap().opened && !os[1].lock().unwrap().opened);
            assert_eq!(local.status().reason, Some(ClipboardError::ConsentRequired));
            assert_eq!(
                remote.status().reason,
                Some(ClipboardError::ConsentRequired)
            );
        }
        Scenario::BlockedFactory => {
            assert!(matches!(host_cleanup, Cleanup::Finished(Err(_))));
            assert!(
                os[0].lock().unwrap().closed,
                "late-opened native owner must close"
            );
            assert_eq!(os[0].lock().unwrap().published.len(), 0);
            assert_eq!(
                host.reap_clipboard(&cleanup, deadline).await.unwrap(),
                host_cleanup
            );
        }
        Scenario::FactoryFailure => assert_eq!(
            host_cleanup,
            Cleanup::Finished(Err(ClipboardError::NativeSetup(
                fr_core::clipboard::PlatformError::Unavailable
            )))
        ),
        Scenario::FactoryPanic => assert_eq!(
            host_cleanup,
            Cleanup::Finished(Err(ClipboardError::Panicked))
        ),
    }
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
    if scenario == Scenario::RetainReceipts {
        drop(viewer);
        let first = remote.take_received();
        let second = remote.take_received();
        assert!(
            matches!(first, Some(Received::Consumed(Some(r))) if r.publication == Publication::SubmittedToOs)
        );
        assert!(
            matches!(second, Some(Received::Consumed(Some(r))) if r.publication == Publication::SubmittedToOs)
        );
        assert_ne!(first, second, "retain distinct committed transfers");
        assert!(remote.take_received().is_none());
    }
}
#[test]
fn native_application_negotiates_workers_both_directions_and_keeps_input_after_stop() {
    run3(|c, h, k| Box::pin(exercise(c, h, k, Scenario::Transfer)));
}
#[test]
fn native_application_bilateral_decline_never_opens_clipboard_or_ends_control() {
    for mode in [Scenario::DeclineHost, Scenario::DeclineViewer] {
        run3(move |c, h, k| Box::pin(exercise(c, h, k, mode)));
    }
}
#[test]
fn native_application_retains_failed_or_panicked_factory_cleanup_and_fences_input() {
    for mode in [Scenario::FactoryFailure, Scenario::FactoryPanic] {
        run3(move |c, h, k| Box::pin(exercise(c, h, k, mode)));
    }
}

#[test]
fn native_application_missing_profile_or_unpolled_service_never_opens_clipboard() {
    for missing in [false, true] {
        run3(move |c, h, cleanup| {
            Box::pin(async move {
                let selected = if missing { capabilities() } else { caps() };
                let (host, viewer) = pair_initialized(&c, &h, selected, |_| {}).await;
                let mut nonce = 80_000;
                let (host, viewer) = support::both(
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
                )
                .await;
                let (mut host, mut viewer) = (host.unwrap(), viewer.unwrap());
                let os = Arc::new(Mutex::new(Os::default()));
                let local = host.configure_clipboard(configured(&os, true, None));
                let remote = viewer.configure_clipboard(configured(&os, true, None));
                if missing {
                    assert!(matches!(local, Err(ClipboardError::NotNegotiated)));
                    assert!(matches!(remote, Err(ClipboardError::NotNegotiated)));
                    // A refused optional profile must not end the original viewing session.
                    host.control_target(Capabilities::default()).unwrap();
                    viewer.control_request(1, Capabilities::default()).unwrap();
                } else {
                    let (local, remote) = (local.unwrap(), remote.unwrap());
                    assert!(matches!(
                        host.configure_clipboard(configured(&os, true, None)),
                        Err(ClipboardError::AlreadyAttached)
                    ));
                    assert!(matches!(
                        viewer.configure_clipboard(configured(&os, true, None)),
                        Err(ClipboardError::AlreadyAttached)
                    ));
                    drop(host.serve_managed_control(
                        Seat::default(),
                        |_| panic!("unpolled host UI"),
                        || panic!("unpolled entropy"),
                        || panic!("unpolled input ticket"),
                    ));
                    drop(viewer.serve_interactive_control(
                        1,
                        Capabilities::default(),
                        fr_client::input::Policy::default(),
                        |_, _| panic!("unpolled viewer UI"),
                        |_| panic!("unpolled input result"),
                    ));
                    assert_eq!(local.status().phase, Phase::Closed);
                    assert_eq!(remote.status().phase, Phase::Closed);
                    assert_eq!(local.set_enabled(true), Err(ClipboardError::Closed));
                    assert_eq!(remote.set_enabled(true), Err(ClipboardError::Closed));
                }
                assert!(!os.lock().unwrap().opened);
                let deadline = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
                assert_eq!(
                    host.reap_clipboard(&cleanup, deadline).await.unwrap(),
                    Cleanup::NotStarted
                );
                assert_eq!(
                    viewer.reap_clipboard(&cleanup, deadline).await.unwrap(),
                    Cleanup::NotStarted
                );
                host.reap_media(&cleanup, deadline).await.unwrap();
                viewer.reap_media(&cleanup, deadline).await.unwrap();
            })
        });
    }
}

#[test]
fn native_application_cleanup_timeout_retains_worker_and_late_open_cannot_publish() {
    run3(|c, h, k| Box::pin(exercise(c, h, k, Scenario::BlockedFactory)));
}

#[test]
fn native_application_retains_final_inflight_receipt_after_native_owner_is_reaped() {
    run3(|c, h, k| Box::pin(exercise(c, h, k, Scenario::RetainReceipts)));
}

#[test]
fn native_application_precontrol_stop_or_disable_declines_without_ending_control() {
    for host_side in [true, false] {
        for mode in [
            Scenario::StopBeforeControl(host_side),
            Scenario::DisableBeforeControl(host_side),
        ] {
            run3(move |c, h, k| Box::pin(exercise(c, h, k, mode)));
        }
    }
}

#[path = "clipboard/reconnect.rs"]
mod reconnect;
