//! Actual supervised child and pipe protocol; the child does not execute HEVC.
#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]
use asupersync::{cx::Cx, runtime::RuntimeBuilder};
use fr_core::ids::CodecConfigurationGeneration;
use fr_media::{
    hevc::{DecoderRecord, HevcGuard},
    worker::{Backend, Configuration, Kind, Role, presentation::X11Target},
};
use frd::worker::{Deadline, Error, Launch, State, Worker};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
fn configuration() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: 1_048_576,
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
fn record() -> DecoderRecord {
    // The same admitted parameter-set fixture as fr-media/hevc_framing.rs.
    let mut bytes = Vec::new();
    for nal in [
        "40010c01ffff01600000030090000003000003003cba0240",
        "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04",
        "4401c0718112",
        "2801ade06702f86753c11ead2f1f6a69",
    ] {
        bytes.extend_from_slice(&[0, 0, 0, 1]);
        for pair in nal.as_bytes().as_chunks::<2>().0 {
            bytes.push(u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap());
        }
    }
    let cfg = configuration();
    let mut guard = HevcGuard::new(cfg.codec().unwrap(), cfg.limits().unwrap(), 4).unwrap();
    guard.validate_annex_b(&bytes, true).unwrap();
    guard.decoder_record().unwrap()
}
fn fixture(mode: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-fit-worker-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(
        &path,
        include_str!("support/fitted_worker_fixture.py").replace("@MODE@", mode),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}
fn deadline(cx: &Cx, millis: u64) -> Deadline {
    Deadline::after(cx, Duration::from_millis(millis)).unwrap()
}
fn runtime() -> asupersync::runtime::Runtime {
    RuntimeBuilder::new()
        .worker_threads(1)
        .blocking_threads(1, 2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
fn launch(path: &std::path::Path) -> Launch {
    Launch::new(path, ":0", None, Role::Present, 71)
        .unwrap()
        .present_fitted_in(X11Target::new(19, 160, 160).unwrap())
        .unwrap()
}
#[test]
fn fitted_bootstrap_keeps_original_decoder_geometry_and_acknowledged_cleanup() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (launch, mut retirement) = launch(&fixture("healthy")).retain_cleanup().unwrap();
        let mut worker =
            Worker::start_decoder(&cx, launch, configuration(), &record(), deadline(&cx, 1000))
                .await
                .unwrap();
        assert_eq!(worker.state(), State::Running);
        worker
            .request(&cx, Kind::Stop, vec![], deadline(&cx, 500))
            .await
            .unwrap();
        assert!(
            worker
                .reap(&cx, deadline(&cx, 500))
                .await
                .unwrap()
                .success()
        );
        drop(worker);
        assert!(
            retirement
                .reap(&cx, deadline(&cx, 500))
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    });
}
#[test]
fn old_worker_changed_echo_and_hung_bootstrap_refuse_with_retained_child_custody() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for mode in ["native-only", "changed-target", "stall"] {
            let (launch, mut retirement) = launch(&fixture(mode)).retain_cleanup().unwrap();
            let error = Worker::start_decoder(
                &cx,
                launch,
                configuration(),
                &record(),
                deadline(&cx, if mode == "stall" { 50 } else { 1000 }),
            )
            .await
            .unwrap_err();
            if mode == "stall" {
                assert_eq!(error, Error::Deadline);
            } else {
                assert_eq!(error, Error::Protocol(fr_media::worker::Error::WrongState));
            }
            assert!(
                retirement
                    .reap(&cx, deadline(&cx, 1000))
                    .await
                    .unwrap()
                    .is_some()
            );
        }
    });
}
#[test]
fn fit_does_not_relax_native_pixel_target_or_capture_role_admission() {
    let target = X11Target::new(19, 160, 160).unwrap();
    assert!(
        Launch::new(
            std::path::Path::new("/usr/bin/false"),
            ":0",
            None,
            Role::Capture,
            71
        )
        .unwrap()
        .present_fitted_in(target)
        .is_err()
    );
    assert!(
        launch(std::path::Path::new("/usr/bin/false"))
            .present_in(target)
            .is_err()
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let launch = Launch::new(
            std::path::Path::new("/usr/bin/false"),
            ":0",
            None,
            Role::Present,
            71,
        )
        .unwrap()
        .present_in(target)
        .unwrap();
        let (launch, mut retirement) = launch.retain_cleanup().unwrap();
        assert_eq!(
            Worker::start_decoder(&cx, launch, configuration(), &record(), deadline(&cx, 500))
                .await
                .unwrap_err(),
            Error::Protocol(fr_media::worker::Error::GeometryChanged)
        );
        assert!(
            retirement
                .reap(&cx, deadline(&cx, 500))
                .await
                .unwrap()
                .is_none()
        );
    });
}
