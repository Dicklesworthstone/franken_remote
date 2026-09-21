//! Real supervised monitor IPC; monitor/codec/permission replies are fixtures.
use super::*;
use crate::{
    session_agent::{
        ApprovalMode, PermissionKind, PermissionStatus, PlatformKind, SessionAgent,
        source::{
            Error as ConsentError,
            desktop::LocalAction,
            prepare::{Error, Setup},
        },
    },
    worker::Retirement,
};
use fr_core::input::{DesktopPoint, InputBounds};
use fr_wire::display::{Catalog, Select};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
};

pub(super) fn agent() -> SessionAgent {
    let mut a = SessionAgent::new(
        ApprovalMode::PromptAlways,
        PlatformKind::LinuxX11,
        12,
        InputBounds::new(DesktopPoint { x: -320, y: 40 }, 320, 240).unwrap(),
    );
    a.permissions_mut()
        .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Granted);
    a
}
pub(super) fn choose(catalog: &Catalog) -> Result<(Select, fr_media::worker::Configuration), ()> {
    assert_eq!(catalog.displays().len(), 2);
    assert_eq!(catalog.displays()[0].x, -320);
    Ok((
        catalog
            .selection(catalog.displays()[0].handle)
            .map_err(|_| ())?,
        codec(),
    ))
}
pub(super) fn setup(rt: &Runtime, mode: &str) -> (Setup, ObservationControl, Retirement, PathBuf) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut payload = String::new();
    for nal in [
        "40010c01ffff01600000030090000003000003003cba0240",
        "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04",
        "4401c0718112",
        "2801ade06702f86753c11ead2f1f6a69",
    ] {
        write!(payload, "{:08x}{nal}", nal.len() / 2).unwrap();
    }
    let path = std::env::temp_dir().join(format!(
        "fr-native-prepare-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let trace = path.with_extension("trace");
    let mut script = include_str!("../../../../../../tests/support/local_source_fixture.py")
        .replace("@MODE@", mode)
        .replace(
            "b\"synthetic-monitor-unit\"",
            &format!("bytes.fromhex('{payload}')"),
        );
    let stamp = format!(
        "with open({:?}, 'a') as log: log.write(str(os.getpid()) + '\\n')\n",
        trace.to_str().unwrap()
    );
    script = script.replace(
        "h, kind, body = record()\nassert kind == 11",
        &(stamp + "h, kind, body = record()\nassert kind == 11"),
    );
    if mode == "stall-discovery" {
        script = script.replace("reply(h, 269", "import time\ntime.sleep(10)\nreply(h, 269");
    }
    if mode == "stall-capture" {
        script = script.replace("    reply(h, 258", &format!("    with open({:?}, 'a') as log: log.write('capture\\n')\n    import time\n    time.sleep(10)\n    reply(h, 258", trace.to_str().unwrap()));
    }
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let control = source_control(rt);
    let (launch, retirement) = Launch::new(&path, ":0", None, WorkerRole::Capture, 91)
        .unwrap()
        .retain_cleanup()
        .unwrap();
    (
        Setup {
            control: control.clone(),
            launch,
            pool: SharedFramePool::new(ProtocolLimits::ABSOLUTE, 32 * 1024 * 1024, 8).unwrap(),
        },
        control,
        retirement,
        trace,
    )
}
async fn retired(r: &mut Retirement) -> Option<asupersync::process::ExitStatus> {
    let cx = Cx::current().unwrap();
    r.reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
        .await
        .unwrap()
}
#[test]
fn native_preparation_registers_the_exact_selected_child_before_first_viewer() {
    let rt = support::runtime();
    rt.block_on(async {
        let (setup, control, mut retirement, trace) = setup(&rt, "normal");
        let mut a = agent();
        let mut prepared = a
            .prepare_native_shared_source(setup, choose, |_, _| Ok(LocalAction::Continue))
            .unwrap()
            .await
            .unwrap();
        let pid = prepared.worker_id().unwrap();
        assert_eq!(
            std::fs::read_to_string(trace).unwrap().trim(),
            pid.to_string()
        );
        assert!(!control.view_ready().unwrap());
        let (publisher, initial) = prepared.parts();
        assert_eq!(
            a.attach_shared_source(publisher),
            Err(ConsentError::AlreadyAttached)
        );
        assert!(control.check().is_ok());
        assert_eq!(
            a.service_shared_sources(|| Ok(950_001))
                .unwrap()
                .outcomes()
                .count(),
            1
        );
        let peer = Box::pin(first(&rt, publisher, initial, 13)).await;
        assert_eq!(publisher.worker_id(), Some(pid));
        close(publisher, vec![peer]).await;
        drop(prepared);
        assert!(control.check().is_err());
        assert!(retired(&mut retirement).await.is_some());
    });
}
#[test]
fn native_preparation_unpolled_drop_fences_without_launching_or_selecting() {
    let rt = support::runtime();
    rt.block_on(async {
        let (setup, control, mut retirement, trace) = setup(&rt, "normal");
        let mut a = agent();
        let run = a
            .prepare_native_shared_source(
                setup,
                |_| panic!("unpolled selection"),
                |_, _| panic!("unpolled event"),
            )
            .unwrap();
        is_send(&run);
        drop(run);
        assert!(control.check().is_err());
        assert!(!trace.exists());
        assert!(retired(&mut retirement).await.is_none());
        // A retired reservation is reusable, but it never restores its source.
        let (setup, next, mut next_retirement, _) = self::setup(&rt, "normal");
        drop(
            a.prepare_native_shared_source(setup, choose, |_, _| Ok(LocalAction::Continue))
                .unwrap()
                .await
                .unwrap(),
        );
        assert!(next.check().is_err());
        assert!(retired(&mut next_retirement).await.is_some());
    });
}
#[test]
fn native_preparation_permission_denial_precedes_spawn_and_does_not_steal_source() {
    let rt = support::runtime();
    rt.block_on(async {
        let (setup, control, mut retirement, trace) = setup(&rt, "normal");
        let mut a = agent();
        a.permissions_mut()
            .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Denied);
        assert!(matches!(
            a.prepare_native_shared_source(setup, choose, |_, _| panic!("denied callback")),
            Err(Error::Consent(ConsentError::NoCapturePermission))
        ));
        assert!(control.check().is_ok());
        assert!(!trace.exists());
        assert!(retired(&mut retirement).await.is_none());
    });
}
#[test]
fn native_preparation_original_reservation_rejects_foreign_owner_during_selector() {
    let rt = support::runtime();
    rt.block_on(async {
        let (setup, control, mut retirement, trace) = setup(&rt, "normal");
        let (mut duplicate, _, mut duplicate_retirement, other_trace) = self::setup(&rt, "normal");
        duplicate.control = control.clone();
        let mut foreign = agent();
        let mut a = agent();
        let owner = control.clone();
        let prepared = a
            .prepare_native_shared_source(
                setup,
                move |catalog| {
                    assert!(matches!(
                        foreign.prepare_native_shared_source(duplicate, choose, |_, _| panic!(
                            "foreign callback"
                        )),
                        Err(Error::Consent(ConsentError::AlreadyAttached))
                    ));
                    assert!(owner.check().is_ok());
                    choose(catalog)
                },
                |_, _| Ok(LocalAction::Continue),
            )
            .unwrap()
            .await
            .unwrap();
        assert!(trace.exists());
        assert!(!other_trace.exists());
        assert!(retired(&mut duplicate_retirement).await.is_none());
        drop(prepared);
        assert!(control.check().is_err());
        assert!(retired(&mut retirement).await.is_some());
    });
}
#[test]
fn native_preparation_local_revocation_interrupts_stalled_discovery_and_capture() {
    let rt = support::runtime();
    rt.block_on(async {
        for mode in ["stall-discovery", "stall-capture"] {
            let (setup, control, mut retirement, trace) = setup(&rt, mode);
            let mut a = agent();
            let clock = control.context();
            let started = now(&clock).unwrap();
            let mut run = Box::pin(
                a.prepare_native_shared_source(setup, choose, move |a, _| {
                    let text = std::fs::read_to_string(&trace).unwrap_or_default();
                    if (mode == "stall-discovery" && !text.is_empty()) || text.contains("capture") {
                        a.permissions_mut().set_permission(
                            PermissionKind::ScreenCapture,
                            PermissionStatus::Denied,
                        );
                    }
                    Ok(LocalAction::Continue)
                })
                .unwrap(),
            );
            assert!(matches!(
                poll_fn(|cx| run.as_mut().poll(cx)).await,
                Err(Error::Consent(ConsentError::NoCapturePermission))
            ));
            assert!(control.check().is_err());
            assert!(crate::media::host_now(&clock).unwrap().as_micros() - started < 2_000_000);
            // The terminal future is still retained, yet authority is already gone.
            drop(run);
            assert!(retired(&mut retirement).await.is_some());
        }
    });
}
#[test]
fn native_preparation_selection_refusal_and_reentrant_revoke_are_terminal() {
    let rt = support::runtime();
    rt.block_on(async {
        for revoke in [false, true] {
            let (setup, control, mut retirement, _) = setup(&rt, "normal");
            let owner = control.clone();
            let mut a = agent();
            let result = a
                .prepare_native_shared_source(
                    setup,
                    move |catalog| {
                        if revoke {
                            owner.revoke();
                        }
                        let (selection, mut configuration) = choose(catalog)?;
                        if !revoke {
                            configuration.width += 2;
                        }
                        Ok((selection, configuration))
                    },
                    |_, _| Ok(LocalAction::Continue),
                )
                .unwrap()
                .await;
            assert!(matches!(result, Err(Error::Media(_))));
            assert!(control.check().is_err());
            assert!(retired(&mut retirement).await.is_some());
        }
    });
}
#[test]
fn native_preparation_caught_selector_unwind_fences_even_retained_future() {
    let rt = support::runtime();
    rt.block_on(async {
        let (setup, control, mut retirement, _) = setup(&rt, "normal");
        let mut a = agent();
        let mut run = Box::pin(
            a.prepare_native_shared_source(
                setup,
                |_| panic!("intentional local selection failure"),
                |_, _| Ok(LocalAction::Continue),
            )
            .unwrap(),
        );
        poll_fn(
            |cx| match catch_unwind(AssertUnwindSafe(|| run.as_mut().poll(cx))) {
                Err(_) => Poll::Ready(()),
                Ok(Poll::Pending) => Poll::Pending,
                Ok(Poll::Ready(_)) => panic!("selector must unwind"),
            },
        )
        .await;
        assert!(control.check().is_err());
        assert!(matches!(
            poll_fn(|cx| run.as_mut().poll(cx)).await,
            Err(Error::Closed)
        ));
        drop(run);
        assert!(retired(&mut retirement).await.is_some());
    });
}
#[test]
fn native_preparation_parked_time_consumes_original_budget_without_spawn() {
    let rt = support::runtime();
    rt.block_on(async {
        let (setup, control, mut retirement, trace) = setup(&rt, "normal");
        let mut a = agent();
        let run = a
            .prepare_native_shared_source(
                setup,
                |_| panic!("expired selection"),
                |_, _| panic!("expired event"),
            )
            .unwrap();
        let cx = Cx::current().unwrap();
        asupersync::time::sleep_until(asupersync::types::Time::from_nanos(
            cx.now().as_nanos() + 2_050_000_000,
        ))
        .await;
        assert!(matches!(run.await, Err(Error::Expired)));
        assert!(control.check().is_err());
        assert!(!trace.exists());
        assert!(retired(&mut retirement).await.is_none());
    });
}
