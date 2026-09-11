use super::*;
use asupersync::{cx::Cx, runtime::RuntimeBuilder};
use fr_core::ids::CodecConfigurationGeneration;
use fr_media::{
    access_unit::FrameId,
    worker::{self, Backend, Configuration, Kind, Role},
};
use frd::worker::{Deadline, Error, Launch, MonitorDiscovery, State};
use std::{future::Future, path::Path, time::Duration};
fn config() -> Configuration {
    Configuration {
        width: 320,
        height: 480,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 4_000_000,
        max_access_unit_bytes: 1_048_576,
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
fn deadline(cx: &Cx) -> Deadline {
    Deadline::after(cx, Duration::from_secs(2)).unwrap()
}
fn launch(screen: &Screen) -> Launch {
    Launch::new(
        Path::new(env!("CARGO_BIN_EXE_fr-media-worker")),
        &screen.name,
        None,
        Role::Capture,
        91,
    )
    .unwrap()
}
fn run<F: FnOnce(Cx) -> Fut, Fut: Future<Output = ()>>(f: F) {
    let runtime = RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .blocking_threads(2, 4)
        .build()
        .unwrap();
    runtime.block_on(async {
        let cx = Cx::current().unwrap();
        asupersync::time::timeout(cx.now(), Duration::from_secs(10), f(cx))
            .await
            .unwrap();
    });
}
// Keep the complete single-worker ordering/effect assertions together.
#[allow(clippy::too_many_lines)]
#[test]
fn supervised_discovery_choice_hevc_and_idle_checks_keep_one_native_worker() {
    run(|cx| async move {
        let screen = Screen::start();
        screen.monitor("fr-right", 320, 0, 320, 480);
        screen.paint(0, 320, 0x00ff_0000);
        screen.paint(320, 320, 0x0000_00ff);
        let mut discovered = MonitorDiscovery::start(&cx, launch(&screen), deadline(&cx))
            .await
            .unwrap();
        let pid = discovered.id().unwrap();
        assert_ne!(pid, std::process::id());
        let cat = discovered.catalog();
        let d = cat.displays().iter().find(|d| d.x == 320).unwrap();
        discovered.check_display(&cx, deadline(&cx)).await.unwrap();
        discovered
            .configure(
                &cx,
                cat.selection(d.handle).unwrap(),
                config(),
                deadline(&cx),
            )
            .await
            .unwrap();
        let mut worker = discovered.into_worker().unwrap();
        assert_eq!(worker.id(), Some(pid));
        assert_eq!(worker.selected_display(), Some(*d));
        assert_eq!(
            worker
                .request(&cx, Kind::CheckMonitor, vec![], deadline(&cx))
                .await
                .unwrap()
                .header
                .kind,
            Kind::MonitorValid
        );
        let mut reply = worker
            .request(
                &cx,
                Kind::Capture,
                worker::capture_payload(FrameId::FIRST, 100, true),
                deadline(&cx),
            )
            .await
            .unwrap();
        while reply.header.kind == Kind::NeedInput {
            reply = worker
                .request(&cx, Kind::Poll, vec![], deadline(&cx))
                .await
                .unwrap();
        }
        assert_eq!(reply.header.kind, Kind::Unit);
        let unit = worker::parse_unit(reply.into_body(), &config().limits().unwrap()).unwrap();
        let mut guard = fr_media::hevc::HevcGuard::new(
            config().codec().unwrap(),
            config().limits().unwrap(),
            4,
        )
        .unwrap();
        guard.validate_length_prefixed(unit.bytes(), true).unwrap();
        let mut decoder = fr_native::HevcDecoder::new(
            config().codec().unwrap(),
            config().limits().unwrap(),
            guard.decoder_record().unwrap().bytes(),
        )
        .unwrap();
        decoder.submit(&unit).unwrap();
        let (_, pixels) = decoder.poll_output().unwrap();
        assert_eq!((pixels.width(), pixels.height()), (320, 480));
        assert!(
            pixels
                .pixels()
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[0] > 240 && p[1] < 10 && p[2] < 10)
        );
        screen.paint(0, 320, 0x0000_ff00);
        let reply = worker
            .request(
                &cx,
                Kind::CaptureIfChanged,
                worker::capture_payload(FrameId::from_raw(2), 200, false),
                deadline(&cx),
            )
            .await
            .unwrap();
        assert_eq!(reply.header.kind, Kind::Unchanged);
        assert_eq!(
            worker::UnchangedCapture::decode(reply.body())
                .unwrap()
                .reference,
            FrameId::FIRST
        );
        screen.paint(320, 320, 0x0000_ff00);
        let reply = worker
            .request(
                &cx,
                Kind::CaptureIfChanged,
                worker::capture_payload(FrameId::from_raw(3), 300, false),
                deadline(&cx),
            )
            .await
            .unwrap();
        assert_eq!(reply.header.kind, Kind::Unit);
        let next = worker::parse_unit(reply.into_body(), &config().limits().unwrap()).unwrap();
        assert!(!next.is_idr());
        decoder.submit(&next).unwrap();
        let (_, pixels) = decoder.poll_output().unwrap();
        assert!(
            pixels
                .pixels()
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[1] > 240 && p[0] < 10 && p[2] < 10)
        );
        worker
            .request(&cx, Kind::Stop, vec![], deadline(&cx))
            .await
            .unwrap();
        assert!(worker.reap(&cx, deadline(&cx)).await.unwrap().success());
        assert!(worker.id().is_none());
    });
}
#[test]
fn replaced_inventory_cannot_reuse_old_aliases_or_survive_awaiting_choice() {
    run(|cx| async move {
        let screen = Screen::start();
        screen.monitor("fr-selected", 0, 0, 320, 480);
        let mut original = MonitorDiscovery::start(&cx, launch(&screen), deadline(&cx))
            .await
            .unwrap();
        let old = original.catalog();
        let chosen = old
            .displays()
            .iter()
            .find(|d| d.pixel_width == 320)
            .unwrap();
        screen.remove("fr-selected");
        screen.monitor("fr-selected", 0, 0, 320, 480);
        assert_eq!(
            original
                .configure(
                    &cx,
                    old.selection(chosen.handle).unwrap(),
                    config(),
                    deadline(&cx)
                )
                .await,
            Err(Error::WorkerRefused(worker::Error::GeometryChanged))
        );
        assert_eq!(original.state(), State::Poisoned);
        original.reap(&cx, deadline(&cx)).await.unwrap();
        let mut fresh = MonitorDiscovery::start(&cx, launch(&screen), deadline(&cx))
            .await
            .unwrap();
        assert_ne!(old.revision(), fresh.catalog().revision());
        assert!(fresh.catalog().find(chosen.handle).is_none());
        assert!(
            fresh
                .configure(
                    &cx,
                    old.selection(chosen.handle).unwrap(),
                    config(),
                    deadline(&cx)
                )
                .await
                .is_err()
        );
        assert_eq!(fresh.state(), State::Starting);
        fresh.check_display(&cx, deadline(&cx)).await.unwrap();
        fresh.stop(&cx, deadline(&cx)).await.unwrap();
        fresh.reap(&cx, deadline(&cx)).await.unwrap();
    });
}
#[test]
fn idle_topology_change_poisoning_does_not_require_another_encoded_picture() {
    run(|cx| async move {
        let screen = Screen::start();
        screen.monitor("fr-selected", 0, 0, 320, 480);
        let mut original = MonitorDiscovery::start(&cx, launch(&screen), deadline(&cx))
            .await
            .unwrap();
        let catalog = original.catalog();
        let d = catalog
            .displays()
            .iter()
            .find(|d| d.pixel_width == 320)
            .unwrap();
        original
            .configure(
                &cx,
                catalog.selection(d.handle).unwrap(),
                config(),
                deadline(&cx),
            )
            .await
            .unwrap();
        let mut worker = original.into_worker().unwrap();
        screen.remove("fr-selected");
        assert_eq!(
            worker
                .request(&cx, Kind::CheckMonitor, vec![], deadline(&cx))
                .await
                .err(),
            Some(Error::WorkerRefused(worker::Error::GeometryChanged))
        );
        assert_eq!(worker.state(), State::Poisoned);
        worker.reap(&cx, deadline(&cx)).await.unwrap();
    });
}
#[test]
fn rejected_geometry_does_not_configure_or_consume_the_original_discovery() {
    run(|cx| async move {
        let screen = Screen::start();
        screen.monitor("fr-selected", 0, 0, 320, 480);
        let mut original = MonitorDiscovery::start(&cx, launch(&screen), deadline(&cx))
            .await
            .unwrap();
        let catalog = original.catalog();
        let d = catalog
            .displays()
            .iter()
            .find(|d| d.pixel_width == 320)
            .unwrap();
        let choice = catalog.selection(d.handle).unwrap();
        assert!(
            original
                .configure(
                    &cx,
                    choice,
                    Configuration {
                        width: 640,
                        ..config()
                    },
                    deadline(&cx)
                )
                .await
                .is_err()
        );
        assert_eq!(original.state(), State::Starting);
        original
            .configure(&cx, choice, config(), deadline(&cx))
            .await
            .unwrap();
        let mut worker = original.into_worker().unwrap();
        worker
            .request(&cx, Kind::Stop, vec![], deadline(&cx))
            .await
            .unwrap();
        worker.reap(&cx, deadline(&cx)).await.unwrap();
    });
}
