#![cfg(target_os = "linux")]
//! Real subprocess/pipe faults with an explicitly synthetic lifecycle source.
use frd::session_monitor::{
    Configuration, Control, Error, Monitor, Selection, State, Status,
    protocol::{self, Reply},
};
use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Command,
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};
static SERIAL: Mutex<()> = Mutex::new(());
fn image() -> PathBuf {
    static IMAGE: OnceLock<PathBuf> = OnceLock::new();
    IMAGE
        .get_or_init(|| {
            let dir = std::env::temp_dir()
                .join(format!("fr-session-monitor-tests-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let image = dir.join("fixture");
            assert!(
                Command::new("cc")
                    .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
                    .arg(
                        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                            .join("tests/session_monitor/fixture.c")
                    )
                    .arg("-o")
                    .arg(&image)
                    .status()
                    .unwrap()
                    .success()
            );
            // The monitor refuses group/other-writable images; cc honours the
            // caller's umask (002 on many desktops gives 0775), so fix the mode.
            std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o755)).unwrap();
            image
        })
        .clone()
}
fn configuration(mode: &str) -> Configuration {
    Configuration {
        image: image(),
        selection: Selection {
            session: mode.into(),
            uid: 0,
            seat: "seat0".into(),
            display: ":0".into(),
        },
    }
}
fn wait(mut ready: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(4);
    while !ready() {
        assert!(Instant::now() < until, "bounded monitor test");
        thread::sleep(Duration::from_millis(2));
    }
}
fn finish(m: &mut Monitor) {
    m.stop();
    wait(|| match m.try_finish() {
        Some(result) => {
            result.unwrap();
            true
        }
        None => false,
    });
}
fn terminal(c: &Control) -> Error {
    wait(|| matches!(c.status(), Status::Stopped(_)));
    let Status::Stopped(e) = c.status() else {
        unreachable!()
    };
    e
}
#[test]
fn real_process_renews_then_local_stop_is_terminal_and_reap_permits_new_epoch() {
    let _serial = SERIAL.lock().unwrap();
    for mode in ["active", "split"] {
        let mut m = Monitor::start(configuration(mode)).unwrap();
        let c = m.control();
        wait(|| c.status() != Status::Opening);
        assert_eq!(c.status(), Status::Active);
        thread::sleep(Duration::from_millis(650));
        assert_eq!(c.status(), Status::Active);
        assert!(matches!(
            Monitor::start(configuration("active")),
            Err(Error::Busy)
        ));
        c.stop();
        assert_eq!(c.status(), Status::Stopped(Error::Stopped));
        finish(&mut m);
        assert_eq!(c.status(), Status::Stopped(Error::Stopped));
    }
}
#[test]
fn responsive_child_cannot_refresh_frozen_logind_evidence() {
    let _serial = SERIAL.lock().unwrap();
    let start = Instant::now();
    let mut m = Monitor::start(configuration("frozen")).unwrap();
    let c = m.control();
    wait(|| c.status() != Status::Opening);
    assert_eq!(c.status(), Status::Active);
    assert_eq!(terminal(&c), Error::EvidenceExpired);
    assert!(start.elapsed() < Duration::from_secs(1));
    finish(&mut m);
}
#[test]
fn stale_wrong_epoch_duplicate_reply_and_wrong_sequence_never_become_fresh_evidence() {
    let _serial = SERIAL.lock().unwrap();
    for (mode, expected) in [
        ("stale", Error::EvidenceExpired),
        ("wrong", Error::Protocol),
        ("sequence", Error::Protocol),
        ("extra", Error::Protocol),
    ] {
        let mut m = Monitor::start(configuration(mode)).unwrap();
        let c = m.control();
        assert_eq!(terminal(&c), expected);
        finish(&mut m);
    }
}
#[test]
fn locked_child_report_is_terminal_even_while_process_and_pipe_are_healthy() {
    let _serial = SERIAL.lock().unwrap();
    let mut m = Monitor::start(configuration("locked")).unwrap();
    let c = m.control();
    wait(|| c.status() != Status::Opening);
    assert_eq!(c.status(), Status::Active);
    assert_eq!(terminal(&c), Error::Native(State::Locked));
    finish(&mut m);
}
#[test]
fn stopped_or_dead_native_process_does_not_hold_host_authority_or_prevent_reaping() {
    let _serial = SERIAL.lock().unwrap();
    for mode in ["hang", "exit"] {
        let start = Instant::now();
        let mut m = Monitor::start(configuration(mode)).unwrap();
        let c = m.control();
        assert!(matches!(terminal(&c), Error::Pipe | Error::ProcessExited));
        finish(&mut m);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
#[test]
fn opening_responses_cannot_extend_the_initial_deadline() {
    let _serial = SERIAL.lock().unwrap();
    let mut m = Monitor::start(configuration("opening")).unwrap();
    let c = m.control();
    assert_eq!(terminal(&c), Error::OpeningExpired);
    finish(&mut m);
}
#[test]
fn dropping_owner_stops_escaped_handle_and_eventually_reaps_original_process() {
    let _serial = SERIAL.lock().unwrap();
    let m = Monitor::start(configuration("active")).unwrap();
    let c = m.control();
    wait(|| c.status() != Status::Opening);
    assert_eq!(c.status(), Status::Active);
    drop(m);
    assert_eq!(c.status(), Status::Stopped(Error::Stopped));
    let mut next = None;
    wait(|| match Monitor::start(configuration("active")) {
        Ok(m) => {
            next = Some(m);
            true
        }
        Err(Error::Busy) => false,
        Err(e) => panic!("{e:?}"),
    });
    finish(next.as_mut().unwrap());
}
#[test]
fn symlink_and_writable_image_refuse_without_reusing_a_previous_live_epoch() {
    use std::os::unix::fs::symlink;
    let _serial = SERIAL.lock().unwrap();
    let original = image();
    let link = original.with_file_name("symlink-image");
    symlink(&original, &link).unwrap();
    let writable = original.with_file_name("writable-image");
    std::fs::copy(&original, &writable).unwrap();
    std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o777)).unwrap();
    for image in [link, writable] {
        let mut cfg = configuration("active");
        cfg.image = image;
        let mut m = Monitor::start(cfg).unwrap();
        assert_eq!(terminal(&m.control()), Error::Image);
        finish(&mut m);
    }
}
#[test]
fn independent_fixed_wire_bytes_bind_role_epoch_sequence_and_exact_selection() {
    let s = Selection {
        session: "c1".into(),
        uid: 1000,
        seat: "seat0".into(),
        display: ":2".into(),
    };
    let bytes = s.encode(13).unwrap();
    assert_eq!(&bytes[..8], b"FRSM\x01\x01\x00\x00");
    assert_eq!(bytes[23], 13);
    assert_eq!(&bytes[24..28], &[0, 0, 3, 232]);
    assert_eq!(Selection::decode(&bytes), Ok((s, 13)));
    for index in [0, 4, 5, 6, 7, 31, 100] {
        let mut bad = bytes;
        bad[index] ^= 128;
        assert!(Selection::decode(&bad).is_err());
    }
    let r = Reply {
        state: State::Locked,
        until_ns: 0,
    };
    let mut expected = [0; 40];
    expected[..4].copy_from_slice(b"FRMR");
    expected[19] = 13;
    expected[27] = 7;
    expected[37] = 2;
    assert_eq!(r.encode(13, 7), Ok(expected));
    assert_eq!(Reply::decode(&expected, 13, 7), Ok(r));
    assert!(Reply::decode(&expected, 14, 7).is_err());
    assert!(Reply::decode(&expected, 13, 8).is_err());
    for index in [0, 28, 36, 38, 39] {
        let mut bad = expected;
        bad[index] ^= 128;
        assert!(Reply::decode(&bad, 13, 7).is_err());
    }
    assert!(protocol::query(0, 1).is_err());
    assert!(protocol::query(1, 0).is_err());
}
