//! Independent selected-display ownership through actual supervised child IPC.
//! The child is an explicit protocol fixture, not native capture or HEVC proof.
#![cfg(target_os = "linux")]
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{CodecConfigurationGeneration, InputLeaseId, RemoteSessionId},
    time::HostDuration,
};
use fr_media::worker::{Backend, Configuration, Role};
use frd::{
    media::{CaptureSource, Error, ObservationControl, discovery::DiscoveredSource, host_now},
    worker::{Deadline, Launch, Retirement},
};
use std::{
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

fn runtime() -> Runtime {
    RuntimeBuilder::new()
        .worker_threads(1)
        .blocking_threads(1, 2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
fn owner(rt: &Runtime, id: u128, control: bool) -> ObservationControl {
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let now = host_now(&cx).unwrap();
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(id),
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(5_000_000),
            ticket_lifetime: HostDuration::from_micros(1_000_000),
        },
    );
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    if control {
        authority.mark_view_ready(now).unwrap();
        authority
            .grant_lease(InputLeaseId::from_raw(1), now)
            .unwrap();
    }
    ObservationControl::new(cx, authority).unwrap()
}
fn configuration() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: 1024 * 1024,
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
async fn discover(c: &ObservationControl, mode: &str) -> (DiscoveredSource, Retirement) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let file = std::env::temp_dir().join(format!(
        "fr-local-source-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(
        &file,
        include_str!("support/local_source_fixture.py").replace("@MODE@", mode),
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (launch, retirement) = Launch::new(&file, ":0", None, Role::Capture, 71)
        .unwrap()
        .retain_cleanup()
        .unwrap();
    (
        DiscoveredSource::start(c, launch).await.unwrap(),
        retirement,
    )
}
async fn collect(retirement: &mut Retirement) {
    let cx = Cx::current().unwrap();
    assert!(
        retirement
            .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap()
            .is_some()
    );
}
async fn selected(c: &ObservationControl, mode: &str) -> (CaptureSource, Retirement) {
    let (d, retirement) = discover(c, mode).await;
    let catalog = d.catalog().unwrap();
    let choice = catalog.selection(catalog.displays()[0].handle).unwrap();
    (
        d.configure_local(choice, configuration())
            .unwrap()
            .await
            .unwrap(),
        retirement,
    )
}

macro_rules! run_local {
    ($rt:ident, $body:expr) => {{
        let $rt = runtime();
        $rt.block_on(async { $body });
    }};
}

#[test]
fn local_choice_retains_original_child_and_discloses_only_selected_alias() {
    run_local!(rt, {
        let c = owner(&rt, 1, false);
        let (d, mut retirement) = discover(&c, "normal").await;
        let pid = d.worker_id();
        let before = d.catalog().unwrap();
        assert_eq!(before.displays().len(), 2);
        let chosen = before.displays()[0];
        assert_ne!(chosen.handle, 201);
        let mut source = d
            .configure_local(before.selection(chosen.handle).unwrap(), configuration())
            .unwrap()
            .await
            .unwrap();
        assert_eq!(source.worker_id(), pid);
        let catalog = source.selected_catalog(&c).unwrap();
        assert_eq!(catalog.revision(), before.revision());
        assert_eq!(catalog.displays(), &[chosen]);
        assert_eq!(catalog.displays()[0].x, -320);
        let first = source.capture_if_changed(&c, false).await.unwrap();
        assert!(!first.is_unchanged());
        source.check_selected_display(&c).await.unwrap();
        let unchanged = source.capture_if_changed(&c, false).await.unwrap();
        assert!(unchanged.is_unchanged());
        assert_eq!(unchanged.frame(), first.frame());
        assert!(!c.view_ready().unwrap());
        drop(source);
        collect(&mut retirement).await;
    });
}
#[test]
fn another_viewer_cannot_own_or_revoke_the_selected_source() {
    run_local!(rt, {
        let c = owner(&rt, 1, false);
        let viewer = owner(&rt, 1, false); // Deliberate equal numeric session ID.
        let (mut source, mut retirement) = selected(&c, "normal").await;
        assert!(matches!(
            source.selected_catalog(&viewer),
            Err(Error::InvalidFrame)
        ));
        assert!(matches!(
            source.capture_if_changed(&viewer, false).await,
            Err(Error::InvalidFrame)
        ));
        viewer.revoke();
        assert!(c.check().is_ok());
        assert!(source.capture_if_changed(&c, false).await.is_ok());
        drop(source);
        collect(&mut retirement).await;
    });
}
#[test]
fn abandoned_unpolled_configuration_revokes_before_original_child_is_collected() {
    run_local!(rt, {
        let c = owner(&rt, 1, false);
        let (d, mut retirement) = discover(&c, "normal").await;
        let catalog = d.catalog().unwrap();
        let future = d
            .configure_local(
                catalog.selection(catalog.displays()[0].handle).unwrap(),
                configuration(),
            )
            .unwrap();
        drop(future);
        assert!(c.check().is_err());
        collect(&mut retirement).await;
    });
}
#[test]
fn stale_native_selection_and_wrong_dimensions_cannot_create_a_source() {
    run_local!(rt, {
        for wrong_dimensions in [false, true] {
            let c = owner(&rt, 1, false);
            let (d, mut retirement) = discover(&c, "normal").await;
            let catalog = d.catalog().unwrap();
            let mut choice = catalog.selection(catalog.displays()[0].handle).unwrap();
            let mut config = configuration();
            if wrong_dimensions {
                config.width = 640;
            } else {
                choice.revision += 1;
            }
            assert!(matches!(
                d.configure_local(choice, config),
                Err(Error::InvalidFrame)
            ));
            assert!(c.check().is_err());
            collect(&mut retirement).await;
        }
    });
}
#[test]
fn configured_input_owner_cannot_become_an_independent_source() {
    run_local!(rt, {
        let c = owner(&rt, 1, true);
        let (d, mut retirement) = discover(&c, "normal").await;
        let catalog = d.catalog().unwrap();
        assert!(matches!(
            d.configure_local(
                catalog.selection(catalog.displays()[0].handle).unwrap(),
                configuration()
            ),
            Err(Error::NotIndependentSource)
        ));
        assert!(c.check().is_err());
        collect(&mut retirement).await;
    });
}
#[test]
fn native_configuration_refusal_and_idle_topology_loss_retire_source() {
    run_local!(rt, {
        let c = owner(&rt, 1, false);
        let (d, mut retirement) = discover(&c, "refuse-configure").await;
        let catalog = d.catalog().unwrap();
        assert!(
            d.configure_local(
                catalog.selection(catalog.displays()[0].handle).unwrap(),
                configuration()
            )
            .unwrap()
            .await
            .is_err()
        );
        assert!(c.check().is_err());
        collect(&mut retirement).await;
        let c = owner(&rt, 2, false);
        let (mut source, mut retirement) = selected(&c, "retired").await;
        assert!(source.check_selected_display(&c).await.is_err());
        assert!(c.check().is_err());
        assert!(source.selected_catalog(&c).is_err());
        drop(source);
        collect(&mut retirement).await;
    });
}
#[test]
fn configuration_deadline_includes_time_before_first_poll() {
    run_local!(rt, {
        let c = owner(&rt, 1, false);
        let (d, mut retirement) = discover(&c, "normal").await;
        let catalog = d.catalog().unwrap();
        let future = d
            .configure_local(
                catalog.selection(catalog.displays()[0].handle).unwrap(),
                configuration(),
            )
            .unwrap();
        // No authority extension; only the original two-second native budget expires.
        std::thread::sleep(Duration::from_millis(2100));
        assert!(c.check().is_ok());
        assert!(future.await.is_err());
        assert!(c.check().is_err());
        collect(&mut retirement).await;
    });
}
#[test]
fn mutable_worker_escape_retires_selected_catalog_and_capture_provenance() {
    run_local!(rt, {
        let c = owner(&rt, 1, false);
        let (mut source, mut retirement) = selected(&c, "normal").await;
        source.worker_mut().abort();
        assert!(c.check().is_err());
        assert!(source.selected_catalog(&c).is_err());
        assert!(source.capture_if_changed(&c, false).await.is_err());
        drop(source);
        collect(&mut retirement).await;
    });
}
