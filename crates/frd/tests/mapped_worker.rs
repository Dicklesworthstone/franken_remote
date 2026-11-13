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
    worker::{
        Kind, Role,
        presentation::{MappedX11Target, X11Target},
    },
};
use frd::worker::{Error, Launch, State, Worker};
use std::path::{Path, PathBuf};
use worker_support::{configuration, deadline, image, runtime};

fn record() -> DecoderRecord {
    let config = configuration();
    let mut guard = HevcGuard::new(config.codec().unwrap(), config.limits().unwrap(), 4).unwrap();
    guard
        .validate_annex_b(&worker_support::hevc_access_unit(true), true)
        .unwrap();
    guard.decoder_record().unwrap()
}

fn mapping() -> MappedX11Target {
    // The shared source's 320x240 picture is smaller than this target. Its
    // original 640x480 display, never encoded dimensions, determines placement.
    MappedX11Target::new(X11Target::new(19, 360, 270).unwrap(), 640, 480).unwrap()
}

fn fixture(mode: &str) -> PathBuf {
    image(
        "fr-mapped-worker",
        include_str!("support/mapped_worker_fixture.py").replace("@MODE@", mode),
    )
}

fn launch(path: &Path) -> Launch {
    Launch::new(path, ":0", None, Role::Present, 71)
        .unwrap()
        .present_mapped_in(mapping())
        .unwrap()
}

#[test]
fn mapped_bootstrap_echoes_original_display_and_reaps_the_exact_child() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (launch, mut retirement) = launch(&fixture("healthy")).retain_cleanup().unwrap();
        let mut worker =
            Worker::start_decoder(&cx, launch, configuration(), &record(), deadline(&cx, 2000))
                .await
                .unwrap();
        assert_eq!(worker.state(), State::Running);
        // A second configuration is not an allowed operation on this owner.
        assert_eq!(
            worker
                .request(
                    &cx,
                    Kind::ConfigureMappedPresentation,
                    vec![],
                    deadline(&cx, 500)
                )
                .await
                .unwrap_err(),
            Error::Protocol(fr_media::worker::Error::WrongRole)
        );
        assert_eq!(worker.state(), State::Running);
        worker
            .request(&cx, Kind::Stop, vec![], deadline(&cx, 1000))
            .await
            .unwrap();
        let status = worker.reap(&cx, deadline(&cx, 1000)).await.unwrap();
        assert!(status.success());
        drop(worker);
        assert_eq!(
            retirement.reap(&cx, deadline(&cx, 1000)).await,
            Ok(Some(status))
        );
    });
}

#[test]
fn incompatible_worker_changed_mapping_and_timeout_never_publish_a_worker() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for (mode, expected) in [
            (
                "native-only",
                Error::Protocol(fr_media::worker::Error::WrongState),
            ),
            (
                "fitted-only",
                Error::Protocol(fr_media::worker::Error::WrongState),
            ),
            (
                "changed-target",
                Error::Protocol(fr_media::worker::Error::WrongState),
            ),
            (
                "changed-display",
                Error::Protocol(fr_media::worker::Error::WrongState),
            ),
            (
                "wrong-epoch",
                Error::Protocol(fr_media::worker::Error::WrongEpoch),
            ),
            (
                "wrong-sequence",
                Error::Protocol(fr_media::worker::Error::WrongSequence),
            ),
            (
                "refused",
                Error::WorkerRefused(fr_media::worker::Error::Unsupported),
            ),
            ("stall", Error::Deadline),
        ] {
            let (launch, mut retirement) = launch(&fixture(mode)).retain_cleanup().unwrap();
            assert_eq!(
                Worker::start_decoder(
                    &cx,
                    launch,
                    configuration(),
                    &record(),
                    deadline(&cx, 2000),
                )
                .await
                .unwrap_err(),
                expected,
                "{mode}"
            );
            assert!(
                retirement
                    .reap(&cx, deadline(&cx, 2000))
                    .await
                    .unwrap()
                    .is_some(),
                "{mode}: the failed original child remains collectable"
            );
        }
    });
}

#[test]
fn mapped_launch_refuses_wrong_role_target_replacement_and_generic_startup() {
    let path = Path::new("/usr/bin/false");
    for role in [Role::Capture, Role::Audio] {
        assert!(
            Launch::new(path, ":0", None, role, 71)
                .unwrap()
                .present_mapped_in(mapping())
                .is_err()
        );
    }
    assert!(launch(path).present_in(mapping().target()).is_err());
    assert!(launch(path).present_fitted_in(mapping().target()).is_err());
    assert!(launch(path).present_mapped_in(mapping()).is_err());
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (launch, mut retirement) = launch(path).retain_cleanup().unwrap();
        assert_eq!(
            Worker::start(&cx, launch, configuration(), deadline(&cx, 1000))
                .await
                .unwrap_err(),
            Error::Protocol(fr_media::worker::Error::WrongState)
        );
        assert_eq!(retirement.reap(&cx, deadline(&cx, 1000)).await, Ok(None));
    });
}
