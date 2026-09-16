//! Two real, independent X11 desktops. Transport below is explicitly a one-record
//! in-memory lane; this does not claim authenticated tailnet/GUI attachment.
use super::*;
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    clipboard::Binding,
    ids::*,
    input::*,
    input_submission::{Capabilities, InputSession as InputOwner},
    time::{HostDuration, HostInstant},
};
use fr_native::clipboard::ClipboardSynchronizer;
use fr_wire::clipboard::{
    Context, Lane, Role,
    session::synchronize::{ReadProgress, Received, SyncError},
    session::{Admission, ChannelSession, Pump, RecordSink, TransportFailure},
};
use std::{
    io::{BufRead, BufReader, Read},
    os::{fd::OwnedFd, unix::net::UnixStream},
    process::{Child, Command, Stdio},
};

struct Desktop {
    process: Child,
    display: String,
}
impl Desktop {
    fn new() -> Self {
        let (reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let process = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-noreset",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .stderr(Stdio::inherit())
            .spawn()
            .expect("real Xvfb required");
        let mut desktop = Self {
            process,
            display: String::new(),
        };
        let mut number = String::new();
        BufReader::new(reader.take(16))
            .read_line(&mut number)
            .unwrap();
        assert!(number.ends_with('\n'));
        let number: u16 = number.trim().parse().unwrap();
        desktop.display = format!(":{number}");
        desktop
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        let _ = self.process.kill();
        self.process.wait().expect("reap test X server");
    }
}
fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
fn owner() -> InputOwner {
    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut authority = SessionAuthority::new(
        credentials.session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(10_000_000),
            ticket_lifetime: HostDuration::from_micros(1_000_000),
        },
    );
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(at(0)).unwrap();
    authority.mark_view_ready(at(0)).unwrap();
    authority.grant_lease(credentials.lease, at(0)).unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, at(0))
        .unwrap();
    InputOwner::new(
        authority,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        at(0),
    )
    .unwrap()
}
fn channel(input: &InputOwner, sender: Role) -> ChannelSession {
    ChannelSession::new(
        input,
        Context {
            scope: Binding {
                session: RemoteSessionId::from_raw(1),
                lease: InputLeaseId::from_raw(2),
            },
            channel: 77,
            sender,
            lane: Lane::Clipboard,
        },
        ProtocolLimits::ABSOLUTE,
        true,
        at(0),
    )
    .unwrap()
}
#[derive(Default)]
struct Record {
    bytes: Option<Vec<u8>>,
    accepted: usize,
}
impl RecordSink for Record {
    fn try_send(&mut self, bytes: &[u8]) -> Result<Admission, TransportFailure> {
        assert!(bytes.len() <= 65_536);
        if self.bytes.is_some() {
            return Ok(Admission::Backpressure);
        }
        self.bytes = Some(bytes.to_vec());
        self.accepted += 1;
        Ok(Admission::Accepted)
    }
}
impl Record {
    fn clear(&mut self) {
        if let Some(mut bytes) = self.bytes.take() {
            bytes.fill(0);
        }
    }
    fn deliver(&mut self, target: &mut ClipboardSynchronizer) -> Option<Stamp> {
        let bytes = self.bytes.as_deref()?;
        let received = target.receive(bytes, || at(0)).unwrap();
        if received == Received::Deferred {
            return None;
        }
        self.clear();
        match received {
            Received::Consumed(Some(receipt)) => {
                assert_eq!(receipt.publication, Publication::SubmittedToOs);
                Some(receipt.stamp)
            }
            Received::Consumed(None) => None,
            other => panic!("unexpected clipboard receipt: {other:?}"),
        }
    }
}
impl Drop for Record {
    fn drop(&mut self) {
        self.clear();
    }
}
struct Pair {
    // Native owners must drop before their test X servers.
    left: ClipboardSynchronizer,
    right: ClipboardSynchronizer,
    left_app: X11Clipboard,
    right_app: X11Clipboard,
    left_input: InputOwner,
    right_input: InputOwner,
    lr: Record,
    rl: Record,
    scratch: Vec<u8>,
    ids: [u128; 2],
    receipts: [Vec<Stamp>; 2],
    desktops: [Desktop; 2],
}
impl Pair {
    fn new() -> Self {
        let desktops = [Desktop::new(), Desktop::new()];
        let left_input = owner();
        let right_input = owner();
        let mut pair = Self {
            left: ClipboardSynchronizer::new(
                channel(&left_input, Role::Host),
                open(&desktops[0].display),
            ),
            right: ClipboardSynchronizer::new(
                channel(&right_input, Role::Controller),
                open(&desktops[1].display),
            ),
            left_app: open(&desktops[0].display),
            right_app: open(&desktops[1].display),
            left_input,
            right_input,
            lr: Record::default(),
            rl: Record::default(),
            scratch: vec![0; 65_536],
            ids: [0, 0],
            receipts: [vec![], vec![]],
            desktops,
        };
        pair.step(); // Both initial empty selections are observed before copies.
        pair
    }
    fn step(&mut self) {
        self.left_app.pump().unwrap();
        self.right_app.pump().unwrap();
        self.left
            .poll(
                &mut self.scratch,
                &mut self.lr,
                || at(0),
                || {
                    self.ids[0] += 1;
                    Ok(self.ids[0])
                },
            )
            .unwrap();
        self.right
            .poll(
                &mut self.scratch,
                &mut self.rl,
                || at(0),
                || {
                    self.ids[1] += 1;
                    Ok(1000 + self.ids[1])
                },
            )
            .unwrap();
        if let Some(stamp) = self.lr.deliver(&mut self.right) {
            self.receipts[1].push(stamp);
        }
        if let Some(stamp) = self.rl.deliver(&mut self.left) {
            self.receipts[0].push(stamp);
        }
    }
    fn until(&mut self, condition: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition(self) {
            self.step();
            assert!(
                Instant::now() < deadline,
                "automatic synchronization stalled"
            );
            std::thread::sleep(Duration::from_micros(100));
        }
    }
    fn text(&mut self, index: usize) -> String {
        let mut observer = open(&self.desktops[index].display);
        observer.begin_read().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            self.step();
            if let Some(text) = observer.poll_read().unwrap() {
                return text.as_str().to_owned();
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_micros(100));
        }
    }
}

#[test]
fn automatic_bidirectional_copies_cross_two_real_desktops_without_echo_or_idle_reads() {
    let _guard = SERIAL.lock().unwrap();
    if display().is_none() {
        return;
    }
    let mut pair = Pair::new();
    for (index, text) in [
        String::new(),
        "automatic 🦀\n\0café".to_owned(),
        "🦀".repeat(262_144),
    ]
    .iter()
    .enumerate()
    {
        publish(&mut pair.left_app, text, (index + 1) as u64);
        pair.until(|p| p.receipts[1].len() == index + 1);
        assert_eq!(pair.text(1), *text);
        // A genuine user copy of equal bytes on the other desktop is NOT an echo.
        publish(&mut pair.right_app, text, (index + 1) as u64);
        pair.until(|p| p.receipts[0].len() == index + 1);
        assert_eq!(pair.text(0), *text);
    }
    let ids = pair.ids;
    let sent = [pair.lr.accepted, pair.rl.accepted];
    for _ in 0..100 {
        pair.step();
    }
    assert_eq!(
        pair.ids, ids,
        "idle and own-publication echoes must not start reads"
    );
    assert_eq!([pair.lr.accepted, pair.rl.accepted], sent);
    assert_eq!(pair.left.retained_channel_bytes(), 0);
    assert_eq!(pair.right.retained_channel_bytes(), 0);
    assert_eq!(pair.ids, [3, 3]);
    assert!(pair.left_input.monitor().deadline(at(0)).is_ok());
    assert!(pair.right_input.monitor().deadline(at(0)).is_ok());
}

#[test]
fn automatic_new_copy_replaces_in_progress_incr_without_sending_old_text() {
    let _guard = SERIAL.lock().unwrap();
    if display().is_none() {
        return;
    }
    let mut pair = Pair::new();
    publish(&mut pair.left_app, &"x".repeat(1_048_576), 1);
    pair.until(|p| p.left_app.active_readers() == 1);
    publish(&mut pair.left_app, "only this newest copy", 2);
    pair.until(|p| p.receipts[1].len() == 1);
    assert_eq!(pair.text(1), "only this newest copy");
    for _ in 0..30 {
        pair.step();
    }
    assert_eq!(pair.receipts[1].len(), 1);
    assert_eq!(pair.ids, [2, 0]);
}

#[test]
fn both_switches_cancel_pending_reads_and_reenable_waits_for_a_new_copy() {
    let _guard = SERIAL.lock().unwrap();
    if display().is_none() {
        return;
    }
    for local in [true, false] {
        let mut pair = Pair::new();
        publish(&mut pair.left_app, &"x".repeat(1_048_576), 1);
        pair.until(|p| p.left_app.active_readers() == 1);
        let switch = if local {
            pair.left.local_switch()
        } else {
            pair.left.peer_switch()
        };
        switch.set_enabled(false);
        switch.set_enabled(true);
        pair.step();
        assert!(!pair.left.is_reading());
        assert!(!pair.left.is_closed());
        for _ in 0..30 {
            pair.step();
        }
        assert_eq!(pair.receipts[1], [] as [Stamp; 0]);
        assert_eq!(
            pair.ids[0], 1,
            "off/on must not replay the interrupted selection"
        );
        assert_eq!(pair.left_app.active_readers(), 0);
        publish(
            &mut pair.left_app,
            "a genuinely new copy after re-enable",
            2,
        );
        pair.until(|p| p.receipts[1].len() == 1);
        assert_eq!(pair.text(1), "a genuinely new copy after re-enable");
        assert!(pair.left_input.monitor().deadline(at(0)).is_ok());
    }
}

#[test]
fn silent_native_read_expires_once_and_never_retries_without_a_change() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let input = owner();
    let mut app = open(&display);
    publish(&mut app, "silent owner", 1);
    let mut sync = ClipboardSynchronizer::new(channel(&input, Role::Host), open(&display));
    let mut record = Record::default();
    let mut scratch = vec![0; 65_536];
    let mut ids = 0;
    sync.poll(
        &mut scratch,
        &mut record,
        || at(0),
        || {
            ids += 1;
            Ok(ids)
        },
    )
    .unwrap();
    assert!(sync.is_reading());
    let result = sync
        .poll(
            &mut scratch,
            &mut record,
            || at(3_000_000),
            || panic!("no retry"),
        )
        .unwrap();
    assert!(matches!(result.read, ReadProgress::Expired));
    for _ in 0..10 {
        sync.poll(
            &mut scratch,
            &mut record,
            || at(3_000_001),
            || panic!("no retry"),
        )
        .unwrap();
    }
    assert_eq!(record.accepted, 0);
    assert_eq!(ids, 1);
    assert!(!sync.is_closed());
}

#[test]
fn revoke_and_caught_clock_panic_close_native_and_wire_without_changing_new_local_copy() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    for panic in [true, false] {
        let input = owner();
        let mut app = open(&display);
        publish(&mut app, "still the user's selection", 1);
        let mut sync = ClipboardSynchronizer::new(channel(&input, Role::Host), open(&display));
        let mut record = Record::default();
        let mut scratch = vec![0; 65_536];
        sync.poll(&mut scratch, &mut record, || at(0), || Ok(1))
            .unwrap();
        if panic {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    sync.poll(
                        &mut scratch,
                        &mut record,
                        || panic!("clock fault"),
                        || Ok(2),
                    )
                    .unwrap();
                }))
                .is_err()
            );
            assert!(input.monitor().deadline(at(0)).is_ok());
        } else {
            input.monitor().revoke();
            assert!(
                sync.poll(&mut scratch, &mut record, || at(0), || Ok(2))
                    .is_err()
            );
        }
        assert!(sync.is_closed());
        assert_eq!(sync.retained_channel_bytes(), 0);
        assert_eq!(app.current_origin().unwrap(), Some(stamp(1)));
        assert_eq!(record.accepted, 0);
    }
}

#[test]
fn an_incoming_begin_after_a_local_copy_survives_that_reads_completion() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let input = owner();
    let peer_input = owner();
    let mut peer = channel(&peer_input, Role::Controller);
    let mut app = open(&display);
    publish(&mut app, &"x".repeat(1_048_576), 1);
    let mut sync = ClipboardSynchronizer::new(channel(&input, Role::Host), open(&display));
    let mut outgoing = Record::default();
    let mut inbound = Record::default();
    let mut scratch = vec![0; 65_536];
    for _ in 0..100 {
        app.pump().unwrap();
        sync.poll(&mut scratch, &mut outgoing, || at(0), || Ok(1))
            .unwrap();
        if app.active_readers() == 1 {
            break;
        }
    }
    assert_eq!(app.active_readers(), 1);
    peer.offer(900, "remote copy after our local revision", None, at(0))
        .unwrap();
    assert_eq!(
        peer.pump(&mut scratch, &mut inbound, || at(0)).unwrap(),
        Pump::RecordAccepted
    );
    assert_eq!(
        sync.receive(inbound.bytes.as_deref().unwrap(), || at(0))
            .unwrap(),
        Received::Consumed(None)
    );
    inbound.clear();
    let mut queued = false;
    for _ in 0..1000 {
        app.pump().unwrap();
        let result = sync
            .poll(
                &mut scratch,
                &mut outgoing,
                || at(0),
                || panic!("one read only"),
            )
            .unwrap();
        if matches!(result.read, ReadProgress::Queued(_)) {
            queued = true;
            break;
        }
    }
    assert!(
        queued,
        "real INCR read should finish without inventing another local revision"
    );
    assert_eq!(
        peer.pump(&mut scratch, &mut inbound, || at(0)).unwrap(),
        Pump::RecordAccepted
    );
    assert_eq!(
        sync.receive(inbound.bytes.as_deref().unwrap(), || at(0))
            .unwrap(),
        Received::Consumed(None)
    );
    inbound.clear();
    assert!(matches!(
        peer.pump(&mut scratch, &mut inbound, || at(0)).unwrap(),
        Pump::ItemAccepted(_)
    ));
    let Received::Consumed(Some(receipt)) = sync
        .receive(inbound.bytes.as_deref().unwrap(), || at(0))
        .unwrap()
    else {
        panic!("newer inbound copy must publish, not fail LocalChanged")
    };
    assert_eq!(receipt.publication, Publication::SubmittedToOs);
    assert_eq!(sync.retained_channel_bytes(), 0);
}

#[test]
fn observation_deadline_is_not_reset_when_text_finishes_or_transport_blocks() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let input = owner();
    let mut app = open(&display);
    publish(&mut app, &"x".repeat(131_072), 1);
    let mut sync = ClipboardSynchronizer::new(channel(&input, Role::Host), open(&display));
    let mut record = Record::default();
    let mut scratch = vec![0; 65_536];
    sync.poll(&mut scratch, &mut record, || at(0), || Ok(1))
        .unwrap();
    let mut queued = None;
    for _ in 0..1000 {
        app.pump().unwrap();
        let progress = sync
            .poll(
                &mut scratch,
                &mut record,
                || at(2_900_000),
                || panic!("no new copy"),
            )
            .unwrap();
        if let ReadProgress::Queued(stamp) = progress.read {
            queued = Some(stamp);
            break;
        }
    }
    let stamp = queued.expect("complete native text is queued within its original lifetime");
    assert_eq!(record.accepted, 1); // Begin admitted; one-record queue now backpressures.
    for _ in 0..10 {
        let progress = sync
            .poll(
                &mut scratch,
                &mut record,
                || at(2_999_999),
                || panic!("no retry"),
            )
            .unwrap();
        assert_eq!(progress.send, Pump::Backpressure);
    }
    record.clear();
    let progress = sync
        .poll(
            &mut scratch,
            &mut record,
            || at(3_000_000),
            || panic!("no retry"),
        )
        .unwrap();
    assert_eq!(progress.send, Pump::CancelAccepted(stamp));
    assert_eq!(sync.retained_channel_bytes(), 0);
    assert!(!sync.is_closed());
}

#[test]
fn a_new_selection_retires_started_outbound_text_before_backpressure_clears() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let input = owner();
    let mut app = open(&display);
    publish(&mut app, "old text already started", 1);
    let mut sync = ClipboardSynchronizer::new(channel(&input, Role::Host), open(&display));
    let mut record = Record::default();
    let mut scratch = vec![0; 65_536];
    let mut ids = 0;
    let mut old_stamp = None;
    for _ in 0..100 {
        app.pump().unwrap();
        let progress = sync
            .poll(
                &mut scratch,
                &mut record,
                || at(0),
                || {
                    ids += 1;
                    Ok(ids)
                },
            )
            .unwrap();
        if let ReadProgress::Queued(stamp) = progress.read {
            old_stamp = Some(stamp);
            break;
        }
    }
    assert!(old_stamp.is_some());
    assert_eq!(record.accepted, 1);
    // Losing the selection is not an empty string and cannot revive old text.
    app.close();
    for _ in 0..10 {
        sync.poll(
            &mut scratch,
            &mut record,
            || at(0),
            || panic!("no selection"),
        )
        .unwrap();
    }
    assert_eq!(sync.retained_channel_bytes(), 0);
    record.clear();
    let result = sync
        .poll(
            &mut scratch,
            &mut record,
            || at(0),
            || panic!("no selection"),
        )
        .unwrap();
    assert_eq!(result.send, Pump::CancelAccepted(old_stamp.unwrap()));
    assert_eq!(ids, 1);
}

#[test]
fn identifier_failure_and_malformed_inbound_records_fail_closed() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let input = owner();
    let mut app = open(&display);
    publish(&mut app, "must not leave native read", 1);
    let mut sync = ClipboardSynchronizer::new(channel(&input, Role::Host), open(&display));
    let mut record = Record::default();
    let mut scratch = vec![0x55; 65_536];
    assert!(matches!(
        sync.poll(&mut scratch, &mut record, || at(0), || Ok(0)),
        Err(SyncError::Identifier)
    ));
    assert!(sync.is_closed());
    assert!(scratch.iter().all(|byte| *byte == 0));
    assert_eq!(
        app.pump().unwrap(),
        0,
        "no content request after failed ID acquisition"
    );
    assert!(input.monitor().deadline(at(0)).is_ok());
    let mut sync = ClipboardSynchronizer::new(channel(&input, Role::Host), open(&display));
    assert!(matches!(
        sync.receive(&[0; 4], || at(0)),
        Err(SyncError::Session(_))
    ));
    assert!(sync.is_closed());
    assert_eq!(sync.retained_channel_bytes(), 0);
    assert_eq!(app.current_origin().unwrap(), Some(stamp(1)));
}

#[test]
fn off_on_during_id_acquisition_cancels_without_destroying_the_channel() {
    let _guard = SERIAL.lock().unwrap();
    let Some(display) = display() else { return };
    let input = owner();
    let mut app = open(&display);
    publish(&mut app, "interrupted copy", 1);
    let mut sync = ClipboardSynchronizer::new(channel(&input, Role::Host), open(&display));
    let switch = sync.peer_switch();
    let mut record = Record::default();
    let mut scratch = vec![0; 65_536];
    let progress = sync
        .poll(
            &mut scratch,
            &mut record,
            || at(0),
            || {
                switch.set_enabled(false);
                switch.set_enabled(true);
                Ok(1)
            },
        )
        .unwrap();
    assert!(matches!(progress.read, ReadProgress::Suspended));
    assert!(!sync.is_closed());
    assert!(!sync.is_reading());
    for _ in 0..10 {
        sync.poll(&mut scratch, &mut record, || at(0), || panic!("no replay"))
            .unwrap();
    }
    assert_eq!(record.accepted, 0);
    assert!(input.monitor().deadline(at(0)).is_ok());
}

#[path = "synchronize/publication.rs"]
mod publication;

#[path = "synchronize/controller.rs"]
mod controller;
