//! The in-process worker owner with a recording presenter in place of the
//! native one: the real `Worker` request path, role checks, deadlines and
//! poisoning, on virtual time. Native decode itself is covered on macOS by
//! fr-native's `macos` tests against `VideoToolbox`.
use super::super::{Deadline, Error, Launch, State, Worker};
use super::LocalPresenter;
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    time::{TimerDriverHandle, VirtualClock},
    types::Time,
};
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy},
    hevc::{DecoderRecord, HevcGuard},
    worker::{self, Backend, Configuration, Kind, overlay},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Configure(Vec<u8>),
    Submit { frame: u64, present: bool },
    Overlay(String),
    Dropped,
}

/// Records every call; fails or overruns the clock on request.
struct Recorder {
    calls: Arc<Mutex<Vec<Call>>>,
    fail_submit: bool,
    overrun: Option<(Arc<VirtualClock>, Time)>,
}
impl LocalPresenter for Recorder {
    fn configure(
        &mut self,
        _configuration: &Configuration,
        record: &DecoderRecord,
    ) -> Result<(), worker::Error> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Configure(record.bytes().to_vec()));
        Ok(())
    }
    fn submit(&mut self, unit: &EncodedAccessUnit, present: bool) -> Result<(), worker::Error> {
        self.calls.lock().unwrap().push(Call::Submit {
            frame: unit.frame().as_raw(),
            present,
        });
        if let Some((clock, to)) = &self.overrun {
            clock.advance_to(*to);
        }
        if self.fail_submit {
            return Err(worker::Error::NativeFailure);
        }
        Ok(())
    }
    fn overlay(&mut self, update: &overlay::Update<'_>) -> Result<(), worker::Error> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Overlay(format!("{update:?}")));
        Ok(())
    }
}
impl Drop for Recorder {
    fn drop(&mut self) {
        self.calls.lock().unwrap().push(Call::Dropped);
    }
}

fn runtime(clock: &Arc<VirtualClock>) -> Runtime {
    RuntimeBuilder::current_thread()
        .with_timer_driver(TimerDriverHandle::with_virtual_clock(Arc::clone(clock)))
        .build()
        .unwrap()
}
fn configuration() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
/// The canonical record of a real 320x240 startup access unit.
fn record() -> DecoderRecord {
    let cfg = CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::INITIAL,
        CodedGeometry::new(&ProtocolLimits::ABSOLUTE, 320, 240, 320, 240, 2).unwrap(),
        ColorInfo::sdr_bt709(),
        GopPolicy::baseline_for_frame_rate(30).unwrap(),
    )
    .unwrap();
    let mut guard = HevcGuard::new(cfg, ProtocolLimits::ABSOLUTE, 4).unwrap();
    let mut au = Vec::new();
    for nal in [
        "40010c01ffff01600000030090000003000003003cba0240",
        "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04",
        "4401c0718112",
        "2801ade06702f86753c11ead2f1f6a69",
    ] {
        au.extend_from_slice(&[0, 0, 0, 1]);
        for hex in nal.as_bytes().as_chunks::<2>().0 {
            au.push(u8::from_str_radix(std::str::from_utf8(hex).unwrap(), 16).unwrap());
        }
    }
    guard.validate_annex_b(&au, true).unwrap();
    guard.decoder_record().unwrap()
}
fn unit(frame: u64) -> Vec<u8> {
    let unit = EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        FrameId::from_raw(frame),
        FrameKind::Idr {
            recovery: RecoveryGeneration::INITIAL,
        },
        CodecConfigurationGeneration::INITIAL,
        1_000,
        vec![0, 0, 0, 1, 0x26, 0x01],
    )
    .unwrap();
    worker::unit_payload(&unit).unwrap()
}
fn recorder(calls: &Arc<Mutex<Vec<Call>>>) -> Recorder {
    failing(calls, false, None)
}
fn failing(
    calls: &Arc<Mutex<Vec<Call>>>,
    fail_submit: bool,
    overrun: Option<(Arc<VirtualClock>, Time)>,
) -> Recorder {
    Recorder {
        calls: Arc::clone(calls),
        fail_submit,
        overrun,
    }
}
async fn start(cx: &Cx, presenter: Recorder) -> Result<Worker, Error> {
    Worker::start_decoder(
        cx,
        Launch::in_process(Box::new(presenter), 7)?,
        configuration(),
        &record(),
        Deadline::after(cx, Duration::from_secs(1))?,
    )
    .await
}

#[test]
fn in_process_launch_refuses_child_only_options() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    assert_eq!(
        Launch::in_process(Box::new(recorder(&calls)), 0).err(),
        Some(Error::InvalidLaunch),
        "a zero epoch never identifies a worker"
    );
    let launch = Launch::in_process(Box::new(recorder(&calls)), 7).unwrap();
    assert_eq!(launch.retain_cleanup().err(), Some(Error::InvalidLaunch));
    // Every refused launch released its presenter without configuring it.
    assert_eq!(*calls.lock().unwrap(), vec![Call::Dropped, Call::Dropped]);
}

#[test]
fn replies_follow_completed_native_calls_in_order() {
    let clock = Arc::new(VirtualClock::new());
    runtime(&clock).block_on(async {
        let cx = Cx::current().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut worker = start(&cx, recorder(&calls)).await.unwrap();
        assert_eq!(worker.state(), State::Running);
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();

        let presented = worker
            .request(&cx, Kind::Present, unit(5), deadline)
            .await
            .unwrap();
        assert_eq!(presented.header.kind, Kind::Presented);
        assert_eq!(presented.body(), 5_u64.to_be_bytes());
        let decoded = worker
            .request(&cx, Kind::Decode, unit(6), deadline)
            .await
            .unwrap();
        assert_eq!(decoded.header.kind, Kind::Decoded);
        assert_eq!(decoded.body(), 6_u64.to_be_bytes());
        assert!(decoded.header.identity.sequence > presented.header.identity.sequence);

        let limits = configuration().limits().unwrap();
        let moved = overlay::encode(&overlay::Update::Move { x: 3, y: 4 }, &limits).unwrap();
        let applied = worker
            .request(&cx, Kind::CursorOverlay, moved, deadline)
            .await
            .unwrap();
        assert_eq!(applied.header.kind, Kind::CursorOverlayApplied);
        assert_eq!(applied.body(), &[] as &[u8]);

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::Configure(record().bytes().to_vec()),
                Call::Submit {
                    frame: 5,
                    present: true
                },
                Call::Submit {
                    frame: 6,
                    present: false
                },
                Call::Overlay("Move".into()),
            ]
        );
    });
}

#[test]
fn unsupported_and_malformed_requests_are_refused_without_native_calls() {
    let clock = Arc::new(VirtualClock::new());
    runtime(&clock).block_on(async {
        let cx = Cx::current().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut worker = start(&cx, recorder(&calls)).await.unwrap();
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        // A pixel snapshot is a presenter request this owner cannot answer.
        assert_eq!(
            worker
                .request(&cx, Kind::ReadPresentation, Vec::new(), deadline)
                .await
                .err(),
            Some(Error::WorkerRefused(worker::Error::Unsupported))
        );
        // A capture request is refused by role before reaching the owner.
        assert_eq!(
            worker
                .request(&cx, Kind::Capture, Vec::new(), deadline)
                .await
                .err(),
            Some(Error::Protocol(worker::Error::WrongRole))
        );
        assert!(matches!(
            worker
                .request(&cx, Kind::Present, vec![1, 2, 3], deadline)
                .await,
            Err(Error::Protocol(_))
        ));
        assert!(matches!(
            worker
                .request(&cx, Kind::CursorOverlay, vec![9], deadline)
                .await,
            Err(Error::Protocol(_))
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![Call::Configure(record().bytes().to_vec())]
        );
    });
}

#[test]
fn native_failure_poisons_and_releases_the_presenter_before_reaping() {
    let clock = Arc::new(VirtualClock::new());
    runtime(&clock).block_on(async {
        let cx = Cx::current().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut worker = start(&cx, failing(&calls, true, None)).await.unwrap();
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        assert_eq!(
            worker
                .request(&cx, Kind::Present, unit(1), deadline)
                .await
                .err(),
            Some(Error::WorkerRefused(worker::Error::NativeFailure))
        );
        assert_eq!(worker.state(), State::Poisoned);
        assert_eq!(calls.lock().unwrap().last(), Some(&Call::Dropped));
        assert_eq!(
            worker
                .request(&cx, Kind::Present, unit(2), deadline)
                .await
                .err(),
            Some(Error::Unavailable),
            "a poisoned owner accepts nothing further"
        );
        let status = worker.reap(&cx, deadline).await.unwrap();
        assert_eq!(status.code(), Some(0));
        assert_eq!(worker.state(), State::Reaped);
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| **c == Call::Dropped)
                .count(),
            1
        );
    });
}

#[test]
fn a_native_call_that_overruns_its_deadline_is_not_a_reply() {
    let clock = Arc::new(VirtualClock::new());
    runtime(&clock).block_on(async {
        let cx = Cx::current().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let overrun = Some((Arc::clone(&clock), Time::from_millis(2_000)));
        let mut worker = start(&cx, failing(&calls, false, overrun)).await.unwrap();
        let deadline = Deadline::after(&cx, Duration::from_millis(500)).unwrap();
        assert_eq!(
            worker
                .request(&cx, Kind::Present, unit(1), deadline)
                .await
                .err(),
            Some(Error::Deadline)
        );
        assert_eq!(worker.state(), State::Poisoned);
        assert_eq!(calls.lock().unwrap().last(), Some(&Call::Dropped));
    });
}

#[test]
fn stop_releases_the_presenter_and_reaps_with_a_zero_status() {
    let clock = Arc::new(VirtualClock::new());
    runtime(&clock).block_on(async {
        let cx = Cx::current().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut worker = start(&cx, recorder(&calls)).await.unwrap();
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        let stopped = worker
            .request(&cx, Kind::Stop, Vec::new(), deadline)
            .await
            .unwrap();
        assert_eq!(stopped.header.kind, Kind::Stopped);
        assert_eq!(worker.state(), State::Stopped);
        assert_eq!(calls.lock().unwrap().last(), Some(&Call::Dropped));
        assert_eq!(worker.reap(&cx, deadline).await.unwrap().code(), Some(0));
        assert_eq!(worker.state(), State::Reaped);
    });
}
