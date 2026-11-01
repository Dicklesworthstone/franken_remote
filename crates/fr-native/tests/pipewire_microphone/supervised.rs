//! The exact daemon supervisor feeds real sibling workers into an independently
//! recorded virtual source. Input establishment and the tone are fixtures; no
//! application-output or child-exit claim is inferred from admission counters.
#[path = "../../../frd/src/microphone_process.rs"]
mod microphone_process;

use super::{
    AudioAccessUnit, HostController, HostInstant, InputSession, OwnedChild, Path, Server, channel,
    configuration, finish, host, input, offer, packets, record, renew, selection,
};
use microphone_process::{Admission, ChildExit, Launch, PacketOutcome, Process, State};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

fn image(variable: &str, built: Option<&str>) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .or_else(|| built.map(PathBuf::from))
        .expect("the actual installed native worker is required; no skip fallback")
}
fn start(server: &Server, owner: &InputSession, origin: Instant) -> Process {
    let scope = channel();
    let launch = Launch::new(
        &image(
            "FR_TEST_MICROPHONE_WORKER",
            option_env!("CARGO_BIN_EXE_fr-microphone-worker"),
        ),
        &image(
            "FR_TEST_OPUS_WORKER",
            option_env!("CARGO_BIN_EXE_fr-opus-worker"),
        ),
        &server.socket,
    )
    .unwrap();
    let mut process = Process::start(
        HostController::from_input(owner),
        microphone_process::Channel {
            parent: scope.parent,
            id: scope.id,
            packet_maximum: scope.packet_maximum,
        },
        &selection(),
        true,
        &configuration(offer()),
        launch,
        || host(origin),
    )
    .unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while process.poll_ready().unwrap().is_none() {
        assert!(Instant::now() < until, "original native readiness deadline");
        std::thread::sleep(Duration::from_millis(1));
    }
    let expected = process.ready_acknowledgement().unwrap().unwrap();
    process
        .acknowledge(|bytes| {
            assert_eq!(bytes, expected);
            Ok(())
        })
        .unwrap();
    let until = Instant::now() + Duration::from_millis(200);
    while process.state() != State::Running {
        assert_eq!(process.state(), State::Committing, "{:?}", process.error());
        assert!(Instant::now() < until, "actual private Commit response");
        std::thread::sleep(Duration::from_millis(1));
    }
    process
}
fn link(server: &Server, process: &Process) -> OwnedChild {
    let mut capture = server.recorder_mode(process.snapshot().unwrap().node_id, "opus");
    let until = Instant::now() + Duration::from_secs(2);
    while !server.directory.join("linked").exists() {
        assert_eq!(process.state(), State::Running, "{:?}", process.error());
        assert!(
            capture.0.try_wait().unwrap().is_none(),
            "see capture.stderr"
        );
        assert!(Instant::now() < until, "real recorder link setup");
        std::thread::sleep(Duration::from_millis(1));
    }
    // The background device owner services its actual graph. This thread never
    // supplies a fabricated graph clock or calls a native output function.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(process.state(), State::Running, "{:?}", process.error());
    capture
}
fn play(
    process: &mut Process,
    owner: &mut InputSession,
    origin: Instant,
    packets: &[AudioAccessUnit],
) {
    let began = Instant::now();
    let mut renew_at = began;
    let mut nonce = 10;
    let mut sent = 0;
    let mut admitted = 0;
    let mut inflight = false;
    while sent < packets.len() {
        let now = Instant::now();
        assert!(
            now.duration_since(began) < Duration::from_secs(5),
            "bounded fixture delivery"
        );
        assert_eq!(
            process.state(),
            State::Running,
            "sent={sent} admitted={admitted} error={:?} snapshot={:?}",
            process.error(),
            process.snapshot()
        );
        if now >= renew_at {
            renew(owner, origin, &mut nonce);
            renew_at = now + Duration::from_millis(500);
        }
        if inflight && let Some(outcome) = process.poll_packet().unwrap() {
            assert_eq!(
                outcome,
                PacketOutcome::Admitted,
                "the live consumer must admit the actual packet"
            );
            admitted += 1;
            inflight = false;
        }
        if !inflight
            && now.duration_since(began) >= Duration::from_millis(u64::try_from(sent).unwrap() * 10)
        {
            let arrived = host(origin);
            let expires = HostInstant::from_micros(arrived.as_micros() + 100_000);
            assert_eq!(
                process.receive_record_until(&record(&packets[sent]), arrived, expires),
                Ok(Admission::Queued)
            );
            sent += 1;
            inflight = true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(admitted, packets.len() - 1);
    assert!(
        began.elapsed() > Duration::from_secs(3),
        "the original input owner must sustain audio beyond the first cap"
    );
    assert!(process.snapshot().unwrap().submitted_frames > 48_000);
}

#[test]
#[ignore = "real PipeWire recorder and installed device/codec workers; explicit native CI"]
fn real_supervised_microphone_reaches_a_host_consumer_and_original_owner_drop_silences_it() {
    let server = Server::start();
    let origin = Instant::now();
    let mut owner = input(host(origin));
    let mut process = start(&server, &owner, origin);
    let control = process.control();
    let ids = process.retirement().child_ids().unwrap();
    assert_ne!(ids.device, ids.codec);
    let mut capture = link(&server, &process);
    let packets = packets();
    play(&mut process, &mut owner, origin, &packets);
    assert_eq!(process.state(), State::Running, "{:?}", process.error());
    assert!(!control.is_stopped());
    let original_until = owner.monitor().deadline(host(origin)).unwrap();
    let retirement = process.retirement();
    let revoked = Instant::now();
    drop(owner);
    // Observe only actual OS exit receipts. The supervisor must notice original
    // authority loss without another network or Process polling turn.
    while !retirement.is_complete() {
        assert!(
            revoked.elapsed() < Duration::from_millis(500),
            "actual sibling waits: {:?}",
            retirement.receipts()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        process.error(),
        Some(microphone_process::Error::Authority(
            fr_core::input_submission::Refusal::Revoked
        ))
    );
    let receipts = retirement.receipts();
    assert_eq!(receipts.device, ChildExit::Reaped);
    assert_eq!(receipts.codec, ChildExit::Reaped);
    assert!(
        host(origin) < original_until,
        "explicit original-owner loss precedes its ordinary expiry"
    );
    assert!(!Path::new(&format!("/proc/{}", ids.device)).exists());
    assert!(!Path::new(&format!("/proc/{}", ids.codec)).exists());
    assert!(control.is_stopped());
    control.stop();
    retirement.stop();
    process.stop();
    assert_eq!(retirement.receipts(), receipts);
    finish(&server, &mut capture);
}
