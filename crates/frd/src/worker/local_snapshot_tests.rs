//! Exact local-worker snapshot requests with a recording presenter. These
//! exercise the real native-task, reply and retirement owners, not a GPU.
use super::*;
use fr_media::worker::presentation_snapshot::{self, Absent, Snapshot, Stamp};
use std::sync::atomic::AtomicUsize;

pub(super) fn stamp() -> Stamp {
    Stamp {
        frame: FrameId::from_raw(17),
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    }
}

#[derive(Clone, Copy)]
enum Reply {
    Pixels(Stamp, u32, u32),
    Absent(Absent),
    Failure(worker::Error),
    ExcessAllocation,
}

struct Presenter {
    recorder: Recorder,
    reply: Reply,
    reads: Arc<Mutex<Vec<(Stamp, usize)>>>,
    allocation: Arc<AtomicUsize>,
}
impl LocalPresenter for Presenter {
    fn configure(
        &mut self,
        configuration: &Configuration,
        record: &DecoderRecord,
    ) -> Result<(), worker::Error> {
        self.recorder.configure(configuration, record)
    }
    fn submit(&mut self, unit: &EncodedAccessUnit, present: bool) -> Result<(), worker::Error> {
        self.recorder.submit(unit, present)
    }
    fn overlay(&mut self, update: &overlay::Update<'_>) -> Result<(), worker::Error> {
        self.recorder.overlay(update)
    }
    fn read_presentation(
        &mut self,
        expected: Stamp,
        maximum_body_bytes: usize,
    ) -> Result<Result<Snapshot, Absent>, worker::Error> {
        self.reads
            .lock()
            .unwrap()
            .push((expected, maximum_body_bytes));
        match self.reply {
            Reply::Pixels(actual, width, height) => {
                let count = presentation_snapshot::pixel_bytes(width, height)?;
                if count + presentation_snapshot::HEADER_BYTES > maximum_body_bytes {
                    return Ok(Err(Absent::TooLarge));
                }
                let pixels = vec![0x7a; count];
                let body = Snapshot::encode(actual, width, height, &pixels)?;
                self.allocation
                    .store(body.as_ptr() as usize, Ordering::Release);
                Snapshot::decode(body).map(Ok)
            }
            Reply::Absent(absent) => Ok(Err(absent)),
            Reply::Failure(error) => Err(error),
            Reply::ExcessAllocation => {
                // Deliberately violate the native allocation contract. Even a
                // valid snapshot body cannot escape its original reply credit.
                let mut body = Vec::with_capacity(maximum_body_bytes + 4);
                body.extend_from_slice(&Snapshot::encode(expected, 2, 1, &[0; 8])?);
                Snapshot::decode(body).map(Ok)
            }
        }
    }
}

struct Evidence {
    calls: Arc<Mutex<Vec<Call>>>,
    reads: Arc<Mutex<Vec<(Stamp, usize)>>>,
    allocation: Arc<AtomicUsize>,
}
impl Evidence {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            reads: Arc::new(Mutex::new(Vec::new())),
            allocation: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn presenter(&self, reply: Reply) -> Presenter {
        Presenter {
            recorder: recorder(&self.calls),
            reply,
            reads: Arc::clone(&self.reads),
            allocation: Arc::clone(&self.allocation),
        }
    }
}

#[test]
fn snapshot_reply_keeps_exact_picture_viewport_and_original_allocation() {
    let source_capacity = presentation_snapshot::HEADER_BYTES + 320 * 240 * 4;
    for (width, height, maximum) in [
        (2, 1, source_capacity),
        (320, 240, source_capacity),
        (640, 480, presentation_snapshot::MAX_BODY_BYTES),
    ] {
        let clock = Arc::new(VirtualClock::new());
        runtime(&clock).block_on(async {
            let cx = Cx::current().unwrap();
            let evidence = Evidence::new();
            let mut worker = start(
                &cx,
                evidence.presenter(Reply::Pixels(stamp(), width, height)),
            )
            .await
            .unwrap();
            let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
            let reply = worker
                .request_with_response_capacity(
                    &cx,
                    Kind::ReadPresentation,
                    stamp().encode().to_vec(),
                    Some(maximum),
                    deadline,
                )
                .await
                .unwrap();
            assert_eq!(reply.header.kind, Kind::PresentationPixels);
            assert_eq!(reply.header.identity.epoch, 7);
            assert_eq!(reply.header.identity.sequence, 1);
            assert_eq!(
                reply.body().as_ptr() as usize,
                evidence.allocation.load(Ordering::Acquire),
                "the response must move the original snapshot allocation"
            );
            let image = Snapshot::decode(reply.into_body()).unwrap();
            assert_eq!(image.stamp(), stamp());
            assert_eq!((image.width(), image.height()), (width, height));
            assert_eq!(image.pixels().len(), (width * height * 4) as usize);
            assert!(image.pixels().iter().all(|&pixel| pixel == 0x7a));
            assert_eq!(*evidence.reads.lock().unwrap(), vec![(stamp(), maximum)]);
            assert_eq!(
                *evidence.calls.lock().unwrap(),
                vec![Call::Configure(record().bytes().to_vec())],
                "snapshot requests do not decode, present or apply an overlay"
            );
            assert_eq!(worker.state(), State::Running);
            stop(&mut worker, &cx).await;
        });
    }
}

#[test]
fn routine_snapshot_absence_keeps_the_original_decoder_usable() {
    for absent in [
        Absent::NoFrame,
        Absent::NotCurrent,
        Absent::Busy,
        Absent::TooLarge,
    ] {
        let clock = Arc::new(VirtualClock::new());
        runtime(&clock).block_on(async {
            let cx = Cx::current().unwrap();
            let evidence = Evidence::new();
            let mut worker = start(&cx, evidence.presenter(Reply::Absent(absent)))
                .await
                .unwrap();
            let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
            let reply = worker
                .request(
                    &cx,
                    Kind::ReadPresentation,
                    stamp().encode().to_vec(),
                    deadline,
                )
                .await
                .unwrap();
            assert_eq!(reply.header.kind, Kind::PresentationAbsent);
            assert_eq!(Absent::decode(reply.body()), Ok(absent));
            assert_eq!(worker.state(), State::Running);
            let presented = worker
                .request(&cx, Kind::Present, unit(18), deadline)
                .await
                .unwrap();
            assert_eq!(presented.header.kind, Kind::Presented);
            assert_eq!(
                presented.header.identity.sequence,
                reply.header.identity.sequence + 1
            );
            assert_eq!(evidence.allocation.load(Ordering::Acquire), 0);
            stop(&mut worker, &cx).await;
        });
    }
}

#[test]
fn default_presenter_has_explicit_snapshot_absence() {
    let clock = Arc::new(VirtualClock::new());
    runtime(&clock).block_on(async {
        let cx = Cx::current().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut worker = start(&cx, recorder(&calls)).await.unwrap();
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        let reply = worker
            .request(
                &cx,
                Kind::ReadPresentation,
                stamp().encode().to_vec(),
                deadline,
            )
            .await
            .unwrap();
        assert_eq!(reply.header.kind, Kind::PresentationAbsent);
        assert_eq!(Absent::decode(reply.body()), Ok(Absent::NoFrame));
        assert_eq!(worker.state(), State::Running);
        stop(&mut worker, &cx).await;
    });
}

#[test]
fn malformed_or_underfunded_snapshot_never_reaches_native_allocation() {
    let clock = Arc::new(VirtualClock::new());
    runtime(&clock).block_on(async {
        let cx = Cx::current().unwrap();
        let evidence = Evidence::new();
        let mut worker = start(&cx, evidence.presenter(Reply::Pixels(stamp(), 2, 1)))
            .await
            .unwrap();
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        for length in [
            0,
            presentation_snapshot::STAMP_BYTES - 1,
            presentation_snapshot::STAMP_BYTES + 1,
        ] {
            assert_eq!(
                worker
                    .request(&cx, Kind::ReadPresentation, vec![0; length], deadline)
                    .await
                    .err(),
                Some(Error::Protocol(worker::Error::ResourceLimit))
            );
        }
        for capacity in [
            0,
            1,
            presentation_snapshot::HEADER_BYTES,
            presentation_snapshot::HEADER_BYTES + 3,
        ] {
            assert_eq!(
                worker
                    .request_with_response_capacity(
                        &cx,
                        Kind::ReadPresentation,
                        stamp().encode().to_vec(),
                        Some(capacity),
                        deadline,
                    )
                    .await
                    .err(),
                Some(Error::Protocol(worker::Error::ResourceLimit))
            );
        }
        assert!(evidence.reads.lock().unwrap().is_empty());
        assert_eq!(evidence.allocation.load(Ordering::Acquire), 0);
        assert_eq!(worker.state(), State::Running);
        let reply = worker
            .request_with_response_capacity(
                &cx,
                Kind::ReadPresentation,
                stamp().encode().to_vec(),
                Some(presentation_snapshot::MAX_BODY_BYTES),
                deadline,
            )
            .await
            .unwrap();
        assert_eq!(reply.header.kind, Kind::PresentationPixels);
        assert_eq!(
            reply.header.identity.sequence, 1,
            "early refusal consumes no worker sequence"
        );
        stop(&mut worker, &cx).await;
    });
}

#[test]
fn reduced_snapshot_credit_reaches_native_unchanged_before_any_pixel_copy() {
    let clock = Arc::new(VirtualClock::new());
    runtime(&clock).block_on(async {
        let cx = Cx::current().unwrap();
        let evidence = Evidence::new();
        let mut worker = start(&cx, evidence.presenter(Reply::Pixels(stamp(), 2, 1)))
            .await
            .unwrap();
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        let too_small = presentation_snapshot::HEADER_BYTES + 4;
        let reply = worker
            .request_with_response_capacity(
                &cx,
                Kind::ReadPresentation,
                stamp().encode().to_vec(),
                Some(too_small),
                deadline,
            )
            .await
            .unwrap();
        assert_eq!(reply.header.kind, Kind::PresentationAbsent);
        assert_eq!(Absent::decode(reply.body()), Ok(Absent::TooLarge));
        assert_eq!(*evidence.reads.lock().unwrap(), vec![(stamp(), too_small)]);
        assert_eq!(evidence.allocation.load(Ordering::Acquire), 0);
        assert_eq!(worker.state(), State::Running);
        let exact = presentation_snapshot::HEADER_BYTES + 8;
        let reply = worker
            .request_with_response_capacity(
                &cx,
                Kind::ReadPresentation,
                stamp().encode().to_vec(),
                Some(exact),
                deadline,
            )
            .await
            .unwrap();
        assert_eq!(reply.header.kind, Kind::PresentationPixels);
        assert_eq!(reply.body().len(), exact);
        assert_eq!(
            *evidence.reads.lock().unwrap(),
            vec![(stamp(), too_small), (stamp(), exact)]
        );
        stop(&mut worker, &cx).await;
    });
}

#[test]
fn returned_snapshot_allocation_cannot_exceed_the_original_reply_credit() {
    let clock = Arc::new(VirtualClock::new());
    runtime(&clock).block_on(async {
        let cx = Cx::current().unwrap();
        let evidence = Evidence::new();
        let mut worker = start(&cx, evidence.presenter(Reply::ExcessAllocation))
            .await
            .unwrap();
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        assert_eq!(
            worker
                .request_with_response_capacity(
                    &cx,
                    Kind::ReadPresentation,
                    stamp().encode().to_vec(),
                    Some(presentation_snapshot::HEADER_BYTES + 8),
                    deadline,
                )
                .await
                .err(),
            Some(Error::Protocol(worker::Error::ResourceLimit)),
            "valid pixel length does not excuse retained capacity above the original budget"
        );
        assert_eq!(worker.state(), State::Poisoned);
        worker.reap(&cx, deadline).await.unwrap();
        assert_eq!(evidence.calls.lock().unwrap().last(), Some(&Call::Dropped));
    });
}

#[test]
fn caller_cannot_widen_the_independent_snapshot_export_ceiling() {
    let clock = Arc::new(VirtualClock::new());
    runtime(&clock).block_on(async {
        let cx = Cx::current().unwrap();
        let evidence = Evidence::new();
        let mut worker = start(&cx, evidence.presenter(Reply::Absent(Absent::NoFrame)))
            .await
            .unwrap();
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        for maximum in [None, Some(usize::MAX)] {
            let reply = worker
                .request_with_response_capacity(
                    &cx,
                    Kind::ReadPresentation,
                    stamp().encode().to_vec(),
                    maximum,
                    deadline,
                )
                .await
                .unwrap();
            assert_eq!(reply.header.kind, Kind::PresentationAbsent);
        }
        assert_eq!(
            *evidence.reads.lock().unwrap(),
            vec![(stamp(), presentation_snapshot::MAX_BODY_BYTES); 2]
        );
        assert_eq!(evidence.allocation.load(Ordering::Acquire), 0);
        stop(&mut worker, &cx).await;
    });
}

#[test]
fn another_configuration_is_absent_before_reading_the_presenter() {
    let clock = Arc::new(VirtualClock::new());
    runtime(&clock).block_on(async {
        let cx = Cx::current().unwrap();
        let evidence = Evidence::new();
        let mut worker = start(&cx, evidence.presenter(Reply::Pixels(stamp(), 2, 1)))
            .await
            .unwrap();
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        let stale = Stamp {
            configuration: CodecConfigurationGeneration::from_raw(99),
            ..stamp()
        };
        let reply = worker
            .request(
                &cx,
                Kind::ReadPresentation,
                stale.encode().to_vec(),
                deadline,
            )
            .await
            .unwrap();
        assert_eq!(reply.header.kind, Kind::PresentationAbsent);
        assert_eq!(Absent::decode(reply.body()), Ok(Absent::NotCurrent));
        assert!(evidence.reads.lock().unwrap().is_empty());
        assert_eq!(evidence.allocation.load(Ordering::Acquire), 0);
        assert_eq!(worker.state(), State::Running);
        stop(&mut worker, &cx).await;
    });
}

#[test]
fn mismatched_pixel_identity_is_terminal_and_never_a_snapshot_reply() {
    for actual in [
        Stamp {
            frame: FrameId::from_raw(18),
            ..stamp()
        },
        Stamp {
            configuration: CodecConfigurationGeneration::from_raw(99),
            ..stamp()
        },
        Stamp {
            recovery: RecoveryGeneration::from_raw(99),
            ..stamp()
        },
    ] {
        let clock = Arc::new(VirtualClock::new());
        runtime(&clock).block_on(async {
            let cx = Cx::current().unwrap();
            let evidence = Evidence::new();
            let mut worker = start(&cx, evidence.presenter(Reply::Pixels(actual, 2, 1)))
                .await
                .unwrap();
            let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
            assert_eq!(
                worker
                    .request(
                        &cx,
                        Kind::ReadPresentation,
                        stamp().encode().to_vec(),
                        deadline
                    )
                    .await
                    .err(),
                Some(Error::Protocol(worker::Error::WrongSequence))
            );
            assert_eq!(worker.state(), State::Poisoned);
            worker.reap(&cx, deadline).await.unwrap();
            assert_eq!(evidence.calls.lock().unwrap().last(), Some(&Call::Dropped));
        });
    }
}

#[test]
fn native_snapshot_fault_and_invalid_size_preserve_failure_and_retire_owner() {
    for (reply, error) in [
        (
            Reply::Failure(worker::Error::NativeFailure),
            worker::Error::NativeFailure,
        ),
        (
            Reply::Pixels(stamp(), u32::MAX, 2),
            worker::Error::ResourceLimit,
        ),
        (Reply::Pixels(stamp(), 0, 1), worker::Error::ResourceLimit),
    ] {
        let clock = Arc::new(VirtualClock::new());
        runtime(&clock).block_on(async {
            let cx = Cx::current().unwrap();
            let evidence = Evidence::new();
            let mut worker = start(&cx, evidence.presenter(reply)).await.unwrap();
            let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
            assert_eq!(
                worker
                    .request(
                        &cx,
                        Kind::ReadPresentation,
                        stamp().encode().to_vec(),
                        deadline
                    )
                    .await
                    .err(),
                Some(Error::WorkerRefused(error))
            );
            assert_eq!(evidence.allocation.load(Ordering::Acquire), 0);
            assert_eq!(worker.state(), State::Poisoned);
            worker.reap(&cx, deadline).await.unwrap();
            assert_eq!(evidence.calls.lock().unwrap().last(), Some(&Call::Dropped));
        });
    }
}

#[test]
fn authority_ending_during_snapshot_fences_reply_before_native_return() {
    let clock = Arc::new(VirtualClock::new());
    runtime(&clock).block_on(async {
        let cx = Cx::current().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (presenter, mut gate) = stalling(&calls, Stage::Snapshot);
        let mut worker = start(&cx, presenter).await.unwrap();
        let authority = AtomicBool::new(true);
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        let mut requesting = Box::pin(worker.request_checked(
            &cx,
            Kind::ReadPresentation,
            stamp().encode().to_vec(),
            None,
            deadline,
            || {
                if authority.load(Ordering::Acquire) {
                    Ok(deadline.time())
                } else {
                    Err(Error::Unavailable)
                }
            },
        ));
        entered(&cx, &mut requesting, &mut gate).await;
        authority.store(false, Ordering::Release);
        assert_eq!(requesting.await.err(), Some(Error::Unavailable));
        no_release(&calls);
        pending_reap(&mut worker, &cx).await;
        gate.release.send(()).unwrap();
        worker.reap(&cx, deadline).await.unwrap();
        assert_eq!(worker.state(), State::Reaped);
    });
}
