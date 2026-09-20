#![cfg(all(target_os = "linux", feature = "linux-displays"))]
//! Real selected-monitor DAMAGE, codec and child IPC; no GPU/visibility claim.
#[path = "display_inventory/support.rs"]
mod support;
use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::{
    access_unit::FrameId,
    worker::{Backend, Configuration},
};
use fr_native::{
    EncodeBackend, HevcEncoder, NativeError,
    capture::{CaptureOutput, ChangeAwareCapture},
};
use support::Screen;

fn configuration() -> Configuration {
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
fn selected(screen: &Screen) -> ChangeAwareCapture {
    let mut inventory = screen.inventory();
    let catalog = inventory.catalog().unwrap();
    let display = catalog.displays().iter().find(|d| d.x == 320).unwrap();
    let surface = inventory.select(display.handle).unwrap();
    let config = configuration();
    let codec = HevcEncoder::new(
        config.codec().unwrap(),
        config.limits().unwrap(),
        EncodeBackend::SoftwareExplicit,
        30,
        config.bitrate,
    )
    .unwrap();
    let mut owner = ChangeAwareCapture::selected(surface, codec);
    assert!(owner.enable_damage_tracking().unwrap());
    owner
}
fn screen() -> Screen {
    let screen = Screen::start();
    screen.monitor("fr-left", 0, 0, 320, 480);
    screen.monitor("fr-right", 320, 0, 320, 480);
    screen.paint(0, 320, 0x00ff_0000);
    screen.paint(320, 320, 0x0000_00ff);
    screen
}
fn capture(owner: &mut ChangeAwareCapture, id: u64, time: u64, force: bool) -> CaptureOutput {
    owner
        .capture(FrameId::from_raw(id), time, force, true)
        .unwrap()
}
fn encoded(owner: &mut ChangeAwareCapture, id: u64, time: u64, force: bool) {
    assert_eq!(capture(owner, id, time, force), CaptureOutput::Submitted);
    let unit = owner.poll_output().unwrap();
    assert_eq!(unit.frame().as_raw(), id);
    if force {
        assert!(unit.is_idr());
    }
}
fn unchanged(result: CaptureOutput, id: u64, reference: u64, time: u64) {
    let CaptureOutput::Unchanged(proof) = result else {
        panic!("expected unchanged selected pixels");
    };
    assert_eq!(proof.candidate.as_raw(), id);
    assert_eq!(proof.reference.as_raw(), reference);
    assert_eq!(proof.observed_micros, time);
}
#[test]
fn selected_monitor_skips_clean_readbacks_but_keeps_periodic_pixel_verification() {
    let screen = screen();
    let mut owner = selected(&screen);
    encoded(&mut owner, 0, 0, true);
    unchanged(capture(&mut owner, 1, 250_000, false), 1, 0, 250_000);
    owner.check_display().unwrap();
    unchanged(capture(&mut owner, 2, 250_001, false), 2, 0, 250_001);
    assert_eq!(owner.stats().readbacks, 2);
    assert_eq!(owner.stats().damage_observations, 1);
    unchanged(capture(&mut owner, 3, 500_000, false), 3, 0, 500_000);
    assert_eq!(owner.stats().readbacks, 3);
    assert_eq!(owner.stats().encoded_submissions, 1);
}
#[test]
fn topology_checks_preserve_damage_instead_of_swallowing_it() {
    let screen = screen();
    let mut owner = selected(&screen);
    encoded(&mut owner, 0, 0, true);
    unchanged(capture(&mut owner, 1, 250_000, false), 1, 0, 250_000);
    screen.paint(320, 320, 0x0000_ff00);
    // These barriers drain all queued events, including DAMAGE. The dirty state
    // must survive independently, even though the next poll sees no new event.
    for _ in 0..3 {
        owner.check_display().unwrap();
    }
    encoded(&mut owner, 2, 250_001, false);
    assert_eq!(owner.stats().readbacks, 3);
    assert_eq!(owner.stats().encoded_submissions, 2);
}
#[test]
fn neighboring_monitor_damage_never_encodes_pixels_outside_the_selection() {
    let screen = screen();
    let mut owner = selected(&screen);
    encoded(&mut owner, 0, 0, true);
    screen.paint(0, 320, 0x0000_ff00);
    owner.check_display().unwrap();
    unchanged(capture(&mut owner, 1, 1, false), 1, 0, 1);
    assert_eq!(owner.stats().readbacks, 2);
    assert_eq!(owner.stats().encoded_submissions, 1);
    unchanged(capture(&mut owner, 2, 250_001, false), 2, 0, 250_001);
    unchanged(capture(&mut owner, 3, 250_002, false), 3, 0, 250_002);
    assert_eq!(owner.stats().damage_observations, 1);
    // No new selected bytes have become an encoded reference. Recovery is still
    // mandatory even when the selected source and DAMAGE state are unchanged.
    encoded(&mut owner, 4, 250_003, true);
    assert_eq!(owner.stats().readbacks, 4);
}
#[test]
fn selected_source_changes_during_pending_encode_survive_completion_barriers() {
    let screen = screen();
    let mut owner = selected(&screen);
    assert_eq!(capture(&mut owner, 0, 0, true), CaptureOutput::Submitted);
    screen.paint(320, 320, 0x00ff_ffff);
    owner.check_display().unwrap();
    assert_eq!(owner.poll_output().unwrap().frame(), FrameId::FIRST);
    encoded(&mut owner, 1, 1, false);
    assert_eq!(owner.stats().encoded_submissions, 2);
    assert_eq!(owner.stats().readbacks, 2);
}
#[test]
fn clean_damage_does_not_hide_same_bounds_monitor_replacement() {
    let screen = screen();
    let mut owner = selected(&screen);
    encoded(&mut owner, 0, 0, true);
    unchanged(capture(&mut owner, 1, 250_000, false), 1, 0, 250_000);
    screen.remove("fr-right");
    screen.monitor("fr-right", 320, 0, 320, 480);
    assert_eq!(
        owner.capture(FrameId::from_raw(2), 250_001, false, true),
        Err(NativeError::GeometryChanged)
    );
    assert_eq!(owner.enable_damage_tracking(), Err(NativeError::Closed));
    assert_eq!(
        owner.capture(FrameId::from_raw(3), 250_002, true, false),
        Err(NativeError::Closed)
    );
    assert_eq!(owner.stats().readbacks, 2);
}
#[test]
fn damage_tracking_cannot_release_a_picture_after_selected_topology_retirement() {
    let screen = screen();
    let mut owner = selected(&screen);
    assert_eq!(capture(&mut owner, 0, 0, true), CaptureOutput::Submitted);
    screen.remove("fr-right");
    screen.monitor("fr-right", 320, 0, 320, 480);
    assert_eq!(
        owner.poll_output().err(),
        Some(NativeError::GeometryChanged)
    );
    assert_eq!(owner.poll_output().err(), Some(NativeError::Closed));
    assert_eq!(owner.stats().encoded_submissions, 1);
    assert_eq!(owner.stats().damage_observations, 0);
}

#[test]
fn selected_worker_observes_damage_after_idle_topology_checks_and_fences_replacement() {
    use fr_media::worker::{self, Identity, Kind, Record};
    use std::process::{Child, Command, Stdio};
    struct Owner(Child);
    impl Drop for Owner {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let screen = screen();
    let mut child = Owner(
        Command::new(env!("CARGO_BIN_EXE_fr-media-worker"))
            .env_clear()
            .env("DISPLAY", &screen.name)
            .args(["--capture", "--parent-pid", &std::process::id().to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let mut output = child.0.stdout.take().unwrap();
    let limits = ProtocolLimits::ABSOLUTE;
    let mut next = 0;
    let mut exchange = |kind, body| {
        let identity = Identity {
            epoch: 9,
            sequence: next,
        };
        next += 1;
        Record::new(kind, identity, body, &limits)
            .unwrap()
            .write(&mut input, &limits)
            .unwrap();
        let reply = Record::read(&mut output, &limits).unwrap().unwrap();
        assert_eq!(reply.header.identity, identity);
        reply
    };
    let catalog = exchange(Kind::DiscoverMonitors, vec![]);
    assert_eq!(catalog.header.kind, Kind::CaptureMonitors);
    let catalog = worker::capture::monitors::decode_catalog(catalog.body(), &limits).unwrap();
    let chosen = catalog.displays().iter().find(|d| d.x == 320).unwrap();
    let body = worker::capture::monitors::encode_configuration(
        configuration(),
        catalog.selection(chosen.handle).unwrap(),
        catalog,
    )
    .unwrap();
    assert_eq!(
        exchange(Kind::ConfigureMonitor, body).header.kind,
        Kind::MonitorReady
    );
    let first = exchange(
        Kind::CaptureIfChanged,
        worker::capture_payload(FrameId::FIRST, 0, true),
    );
    assert_eq!(first.header.kind, Kind::Unit);
    assert!(
        worker::parse_unit(first.into_body(), &limits)
            .unwrap()
            .is_idr()
    );
    let idle = exchange(
        Kind::CaptureIfChanged,
        worker::capture_payload(FrameId::from_raw(1), 250_000, false),
    );
    assert_eq!(idle.header.kind, Kind::Unchanged);
    screen.paint(320, 320, 0x0000_ff00);
    assert_eq!(
        exchange(Kind::CheckMonitor, vec![]).header.kind,
        Kind::MonitorValid
    );
    let changed = exchange(
        Kind::CaptureIfChanged,
        worker::capture_payload(FrameId::from_raw(2), 250_001, false),
    );
    assert_eq!(changed.header.kind, Kind::Unit);
    assert_eq!(
        worker::parse_unit(changed.into_body(), &limits)
            .unwrap()
            .frame()
            .as_raw(),
        2
    );
    screen.remove("fr-right");
    screen.monitor("fr-right", 320, 0, 320, 480);
    let closed = exchange(
        Kind::CaptureIfChanged,
        worker::capture_payload(FrameId::from_raw(3), 250_002, false),
    );
    assert_eq!(closed.header.kind, Kind::Refused);
    assert_eq!(
        closed.body(),
        (worker::Error::GeometryChanged as u16).to_be_bytes()
    );
    assert!(!child.0.wait().unwrap().success());
}
