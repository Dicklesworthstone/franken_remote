//! Actual supervised child and pipe protocol; the child does not execute HEVC.
#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]
#[path = "support/executable.rs"]
mod executable;
#[path = "support/worker.rs"]
#[allow(dead_code)]
mod worker_support;
use asupersync::cx::Cx;
use fr_media::{
    hevc::{DecoderRecord, HevcGuard},
    worker::{Kind, Role, presentation::X11Target},
};
use frd::worker::{Error, Launch, State, Worker};
use std::path::PathBuf;
use worker_support::{configuration, deadline, image, runtime};
fn record() -> DecoderRecord {
    // The same admitted parameter-set fixture as fr-media/hevc_framing.rs.
    let bytes = crate::worker_support::hevc_access_unit(true);
    let cfg = configuration();
    let mut guard = HevcGuard::new(cfg.codec().unwrap(), cfg.limits().unwrap(), 4).unwrap();
    guard.validate_annex_b(&bytes, true).unwrap();
    guard.decoder_record().unwrap()
}
fn fixture(mode: &str) -> PathBuf {
    image(
        "fr-fit-worker",
        include_str!("support/fitted_worker_fixture.py").replace("@MODE@", mode),
    )
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
