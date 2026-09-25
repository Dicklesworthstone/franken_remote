//! Real UDP/TLS and supervised children through both PUBLIC bootstrap APIs.
//! The child replays a recorded HEVC IDR; discovery, decoder replies, native
//! input and platform visibility remain explicit fixtures, not hardware proof.
use super::*;
use crate::{
    input_agent::{Driver, Seat},
    session_startup::{
        self, HostControlState, ObserverPolicy, PublisherPolicy, ViewerControlState, now,
        running::tests::{pair_initialized, run},
        tests::support,
    },
    worker::{Deadline, Launch},
};
use asupersync::cx::Cx;
use fr_core::{
    ids::*,
    input::*,
    input_submission::{Capability, InputSink, Operation, PlatformError, Submission},
};
use fr_media::{
    freshness::ClockPolicy,
    worker::{Backend, Configuration, Role as WorkerRole},
};
use fr_wire::negotiation::{Capability as WireCapability, Offer};
use std::{
    cell::Cell,
    future::Future,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::Poll,
    time::Duration,
};

fn capabilities() -> Vec<WireCapability> {
    let mut c: Vec<_> = [
        fr_wire::display::CAPABILITY,
        decoder::CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
        attachment::INPUT_CAPABILITY,
        crate::input_quic::grant::CAPABILITY,
        clock::CAPABILITY,
        presented::CAPABILITY,
    ]
    .into_iter()
    .map(|name| WireCapability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect();
    c.sort_by(|a, b| a.name.cmp(&b.name));
    c
}
fn image() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "fr-native-control-{}-{}.py",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&p, include_str!("worker_fixture.py")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    p
}
fn launch(role: WorkerRole) -> Launch {
    Launch::new(&image(), ":0", None, role, 155).unwrap()
}
#[allow(clippy::unnecessary_wraps)]
fn config(d: Display) -> Result<Configuration, ()> {
    Ok(Configuration {
        width: d.pixel_width,
        height: d.pixel_height,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: fr_core::limits::ProtocolLimits::ABSOLUTE
            .max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    })
}
#[test]
fn explicit_control_profile_never_upgrades_or_omits_a_required_boundary() {
    let offer = Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::RequestControl,
        limits: fr_core::limits::ProtocolLimits::ABSOLUTE,
        capabilities: capabilities(),
    };
    let selection = offer.select().unwrap();
    assert!(profile(&selection, true));
    assert!(!profile(&selection, false));
    for index in 0..selection.capabilities.len() {
        let mut missing = selection.clone();
        missing.capabilities.remove(index);
        assert!(!profile(&missing, true));
        let mut wrong = selection.clone();
        wrong.capabilities[index].version = 2;
        assert!(!profile(&wrong, true));
    }
    let mut observer = selection;
    observer.role = Role::Observe;
    assert!(profile(&observer, false));
    assert!(!profile(&observer, true));
}
#[test]
fn host_offer_makes_control_optional_and_never_upgrades_an_observer() {
    use fr_client::native::{control_offer, observation_offer};
    let observe_only = host_offer(false);
    assert!(observe_only.validate().is_ok());
    assert_eq!(observe_only.capabilities.len(), 5);
    // Only remote-cursor forwarding is optional; bootstrap stays mandatory.
    assert!(
        observe_only
            .capabilities
            .iter()
            .all(|c| c.required != (c.name == fr_wire::cursor::CAPABILITY))
    );
    // Without control, a controller is a typed required-capability refusal.
    assert!(matches!(
        observe_only.intersect(&control_offer()),
        Err(fr_wire::negotiation::Error::RequiredCapability)
    ));
    let control = host_offer(true);
    assert!(control.validate().is_ok());
    assert_eq!(control.capabilities.len(), 9);
    assert_eq!(
        control.capabilities.iter().filter(|c| c.required).count(),
        4
    );
    let controller = control
        .intersect(&control_offer())
        .unwrap()
        .select()
        .unwrap();
    assert!(profile(&controller, true));
    let observer = control
        .intersect(&observation_offer())
        .unwrap()
        .select()
        .unwrap();
    assert!(profile(&observer, false));
    assert!(!profile(&observer, true));
    assert!(
        observer
            .capabilities
            .iter()
            .all(|c| c.name != attachment::INPUT_CAPABILITY)
    );
}
#[test]
fn clipboard_is_offered_optionally_only_with_control_and_the_local_enable() {
    use crate::session_startup::clipboard::selected;
    use fr_client::native::{control_offer, control_offer_with_clipboard};
    // Unchanged without the operator's enable, and never on an observe-only host.
    assert_eq!(host_offer_with(true, false), host_offer(true));
    assert_eq!(host_offer_with(false, true), host_offer(false));
    let host = host_offer_with(true, true);
    assert!(host.validate().is_ok());
    assert_eq!(host.capabilities.len(), 12);
    assert_eq!(host.capabilities.iter().filter(|c| c.required).count(), 4);
    let client = control_offer_with_clipboard();
    assert!(client.validate().is_ok());
    // Both enabled: the selection carries the whole clipboard profile.
    let both = host.intersect(&client).unwrap();
    client.check_host(&both).unwrap();
    let both = both.select().unwrap();
    assert!(profile(&both, true));
    assert!(selected(&both).is_ok());
    // A host without the enable: control still negotiates; clipboard is absent
    // by type (NotNegotiated), never a refused connection.
    let absent = host_offer(true).intersect(&client).unwrap();
    client.check_host(&absent).unwrap();
    let absent = absent.select().unwrap();
    assert!(profile(&absent, true));
    assert_eq!(
        selected(&absent),
        Err(crate::clipboard_quic::Error::NotNegotiated)
    );
    // A client that did not ask: the host's optional offer is simply dropped.
    let unasked = host.intersect(&control_offer()).unwrap().select().unwrap();
    assert!(profile(&unasked, true));
    assert!(selected(&unasked).is_err());
}
#[test]
fn shipped_client_control_offer_selects_exactly_the_host_control_profile() {
    use fr_client::native::{control_offer, observation_offer};
    let control = control_offer()
        .intersect(&control_offer())
        .unwrap()
        .select()
        .unwrap();
    assert!(profile(&control, true));
    assert!(!profile(&control, false));
    let observe = observation_offer()
        .intersect(&observation_offer())
        .unwrap()
        .select()
        .unwrap();
    assert!(profile(&observe, false));
    assert!(!profile(&observe, true));
    // The shared constant is the one the host's grant exchange negotiates.
    assert_eq!(
        crate::input_quic::grant::CAPABILITY,
        fr_wire::control::GRANT_CAPABILITY
    );
}
#[test]
fn cancelled_and_expired_control_bootstrap_cannot_start_native_work() {
    for expiry in [false, true] {
        run(|c, h| async move {
            let (mut host, viewer) = pair_initialized(&c, &h, capabilities(), |_| {}).await;
            let control = host.observation().unwrap();
            let a = host.publish_controlled_display(
                launch(WorkerRole::Capture),
                PublisherPolicy {
                    timeout: Duration::from_millis(1),
                    ..PublisherPolicy::default()
                },
                |_| panic!("unpolled configure"),
                || panic!("unpolled entropy"),
            );
            let b = viewer.observe_for_control(
                launch(WorkerRole::Present),
                ObserverPolicy {
                    timeout: Duration::from_millis(1),
                    ..ObserverPolicy::default()
                },
                ClockPolicy::default(),
                |_| panic!("unpolled choice"),
            );
            if expiry {
                asupersync::time::sleep(h.now(), Duration::from_millis(5)).await;
                assert!(matches!(
                    a.await,
                    Err(session_startup::PublisherError::Expired)
                ));
                assert!(matches!(
                    b.await,
                    Err(session_startup::ObserverError::Expired)
                ));
            } else {
                drop(a);
                drop(b);
            }
            assert!(control.check().is_err());
            assert!(c.checkpoint().is_err());
        });
    }
}
#[test]
fn control_bootstrap_refuses_missing_proof_before_native_discovery() {
    run(|c, h| async move {
        let mut caps = capabilities();
        caps.retain(|v| v.name != presented::CAPABILITY);
        let (host, viewer) = pair_initialized(&c, &h, caps, |_| {}).await;
        let a = host.publish_controlled_display(
            launch(WorkerRole::Capture),
            PublisherPolicy::default(),
            |_| panic!("missing proof configure"),
            || panic!("missing proof entropy"),
        );
        let b = viewer.observe_for_control(
            launch(WorkerRole::Present),
            ObserverPolicy::default(),
            ClockPolicy::default(),
            |_| panic!("missing proof choice"),
        );
        assert!(matches!(
            a.await,
            Err(session_startup::PublisherError::InvalidConfiguration)
        ));
        assert!(matches!(
            b.await,
            Err(session_startup::ObserverError::InvalidConfiguration)
        ));
    });
}
struct Sink(Arc<Mutex<Vec<Operation>>>);
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        self.0.lock().unwrap().push(op);
        Submission::Submitted
    }
}
fn run3<F: FnOnce(Cx, Cx, Cx) -> Fut, Fut: Future<Output = ()>>(f: F) {
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    let h = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    let cleanup = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    runtime.block_on(async {
        asupersync::time::timeout(cleanup.now(), Duration::from_secs(12), f(c, h, cleanup))
            .await
            .unwrap();
    });
}
#[allow(clippy::too_many_lines)]
async fn exercise(c: Cx, h: Cx, cleanup: Cx, consent: bool, visible: bool) {
    let (mut host, viewer) = pair_initialized(&c, &h, capabilities(), |_| {}).await;
    let control = host.observation().unwrap();
    assert!(!control.view_ready().unwrap());
    let mut nonce_counter = 6000;
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
    assert_eq!(host.display(), viewer.display());
    let caps = Capabilities::default().with(Capability::Keys);
    let target = host.control_target(caps).unwrap();
    let request = viewer.control_request(1, caps).unwrap();
    assert_eq!(request.target, target);
    assert_eq!(target.bounds.origin(), DesktopPoint { x: -320, y: 0 });
    let host_pid = host.worker_id();
    let viewer_pid = viewer.worker_id();
    assert!(
        !control.view_ready().unwrap(),
        "bootstrap decode fabricated visibility"
    );
    let stop = viewer.control();
    let seat = Seat::default();
    let effects = Arc::new(Mutex::new(Vec::new()));
    let (tx, mut rx) = asupersync::channel::mpsc::channel::<Driver>(1);
    let host_done = Cell::new(false);
    let viewer_done = Cell::new(false);
    let receipts = Cell::new(0);
    let mut shown = None;
    let mut sent_actions = 0;
    let mut ticket = 8000;
    let mut active = None;
    let mut approved = false;
    let ((hr, vr), ()) = Box::pin(support::both(
        support::both(
            async {
                let result = host
                    .serve_accepting_control(
                        seat.clone(),
                        |state| {
                            if let HostControlState::Pending(mut pending) = state
                                && consent
                                && pending.request().is_some()
                                && pending.native_status().is_none()
                                && pending.view_ready()?
                            {
                                let sink_effects = effects.clone();
                                let driver = pending.approve(
                                    target,
                                    || {
                                        Some((
                                            InputLeaseId::from_raw(19),
                                            InputTicketId::from_raw(23),
                                        ))
                                    },
                                    move || Ok(Sink(sink_effects)),
                                    |_| true,
                                )?;
                                assert!(tx.try_send(driver).is_ok());
                                approved = true;
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
                    .serve_requesting_control(
                        1,
                        caps,
                        fr_client::input::Policy::default(),
                        |state, event| {
                            match state {
                                ViewerControlState::Requesting(pending) => {
                                    pending
                                        .confirm_mapping(request.parent, target.view)
                                        .unwrap();
                                    if visible
                                        && let Some(p) = pending.presentation()
                                        && shown != Some(p.frame)
                                    {
                                        pending.visible(p.frame.as_raw()).unwrap();
                                        shown = Some(p.frame);
                                    }
                                }
                                ViewerControlState::Controlled(input) => {
                                    assert!(visible && consent);
                                    let at = *active.get_or_insert_with(|| now(&c).unwrap());
                                    if let Some(p) = event {
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
                                    if receipts.get() == 2 && now(&c).unwrap() >= at + 3_100_000 {
                                        control.revoke();
                                        stop.stop();
                                    }
                                }
                                ViewerControlState::Observing => panic!("lost explicit control"),
                            }
                            Ok(())
                        },
                        |_| receipts.set(receipts.get() + 1),
                    )
                    .await;
                viewer_done.set(true);
                control.revoke();
                result
            },
        ),
        async {
            let mut driver = None;
            let mut done = false;
            loop {
                if driver.is_none()
                    && let Ok(d) = rx.try_recv()
                {
                    driver = Some(d);
                }
                if let Some(d) = &mut driver {
                    std::future::poll_fn(|task| {
                        if !done && let Poll::Ready(result) = Pin::new(&mut *d).poll(task) {
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
    assert!(hr.is_err() && vr.is_err());
    if visible && consent {
        assert_eq!(receipts.get(), 2, "host {hr:?} viewer {vr:?}");
        assert!(approved);
        assert!(host.presentation_reports() > 1);
        assert!(viewer.presentation_reports() > 1);
        assert!(host.statistics().unchanged_observations > 1);
    } else {
        assert_eq!(receipts.get(), 0);
        assert!(!approved);
        assert!(effects.lock().unwrap().is_empty());
    }
    assert_eq!(host.worker_id(), host_pid);
    assert_eq!(viewer.worker_id(), viewer_pid);
    assert_eq!(host.statistics().encoded_updates, 0);
    assert_eq!(viewer.statistics().decoded, 0);
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
}
#[test]
fn public_bootstraps_join_real_grant_and_static_source_renewal_without_manual_channels() {
    run3(|c, h, cleanup| exercise(c, h, cleanup, true, true));
}
#[test]
fn public_control_bootstrap_never_substitutes_first_decode_for_visibility() {
    run3(|c, h, cleanup| exercise(c, h, cleanup, true, false));
}
#[test]
fn public_control_bootstrap_never_substitutes_presentation_for_consent() {
    run3(|c, h, cleanup| exercise(c, h, cleanup, false, true));
}

mod dispatch;
mod handoff;
mod interactive;

mod clipboard;

mod sharing;
