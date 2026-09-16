use super::*;
use fr_wire::clipboard::session::egress::{Egress, Transport};
#[derive(Default)]
struct Handoff {
    permit: Option<Egress>,
    bytes: Vec<u8>,
}
impl RecordSink for Handoff {
    fn try_send(&mut self, _: &[u8]) -> Result<Admission, TransportFailure> {
        panic!("asynchronous path must include original-operation metadata")
    }
    fn try_send_checked(
        &mut self,
        bytes: &[u8],
        permit: Egress,
    ) -> Result<Admission, TransportFailure> {
        self.permit = Some(permit);
        self.bytes = bytes.to_vec();
        Ok(Admission::Accepted)
    }
}
fn handoff(c: &mut ClipboardChannel, now: u64) -> Handoff {
    let mut h = Handoff::default();
    c.pump(&mut vec![0; 17000], &mut h, || at(now)).unwrap();
    h
}
#[test]
fn queued_commit_keeps_original_deadline_after_sender_finishes() {
    let input = owner(10_000_000);
    let mut c = session(&input, Role::Host);
    c.offer(1, "", None, at(50)).unwrap();
    let begin = handoff(&mut c, 100);
    let commit = handoff(&mut c, 1000);
    assert_eq!(c.retained_bytes(), 0);
    for h in [begin, commit] {
        let p = h.permit.unwrap();
        assert_eq!(p.context(), context(Role::Host));
        assert_eq!(p.deadline(), at(3_000_050));
        p.check(at(3_000_049)).unwrap();
        assert_eq!(p.check(at(3_000_050)), Err(Error::Expired.into()));
    }
}
#[test]
fn queued_bytes_do_not_survive_local_copy_switch_cycles_or_original_owner_drop() {
    for fault in 0..5 {
        let input = owner(10_000_000);
        let mut c = session(&input, Role::Host);
        c.offer(1, "old", None, at(0)).unwrap();
        let h = handoff(&mut c, 0);
        let p = h.permit.unwrap();
        match fault {
            0 => {
                c.offer(2, "new", None, at(1)).unwrap();
            }
            1 | 2 => {
                let s = if fault == 1 {
                    c.local_switch()
                } else {
                    c.peer_switch()
                };
                s.set_enabled(false);
                s.set_enabled(true);
            }
            3 => c.close(),
            _ => drop(input),
        }
        assert!(p.check(at(2)).is_err());
    }
}
#[test]
fn native_revision_invalidates_even_an_already_handed_off_commit() {
    let input = owner(10_000_000);
    let mut c = session(&input, Role::Host);
    c.offer(1, "", None, at(0)).unwrap();
    handoff(&mut c, 0);
    let commit = handoff(&mut c, 0).permit.unwrap();
    c.observe(at(1))
        .unwrap()
        .native_change(None, at(1))
        .unwrap();
    assert_eq!(commit.check(at(1)), Err(Error::LocalChanged.into()));
}
#[test]
fn network_stop_during_native_preparation_prevents_publication_not_input() {
    struct StopInPrepare {
        gate: Transport,
        calls: usize,
    }
    impl ClipboardSink for StopInPrepare {
        fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
            self.gate.close();
            Ok(())
        }
        fn publish(&mut self, _: &str, _: Stamp) -> Publication {
            self.calls += 1;
            Publication::SubmittedToOs
        }
    }
    let input = owner(10_000_000);
    let peer = owner(10_000_000);
    let mut c = session(&input, Role::Host);
    let mut source = session(&peer, Role::Controller);
    let mut platform = StopInPrepare {
        gate: c.transport(),
        calls: 0,
    };
    source.offer(1, "", None, at(0)).unwrap();
    let b = handoff(&mut source, 0);
    c.receive(&b.bytes, &mut platform, || at(0)).unwrap();
    let b = handoff(&mut source, 0);
    assert!(c.receive(&b.bytes, &mut platform, || at(0)).is_err());
    assert_eq!(platform.calls, 0);
    input.monitor().deadline(at(1)).unwrap();
}
#[test]
fn cancellation_can_release_peer_while_switches_are_disabled() {
    let input = owner(10_000_000);
    let mut c = session(&input, Role::Host);
    c.offer(1, "old", None, at(0)).unwrap();
    handoff(&mut c, 0);
    c.local_switch().set_enabled(false);
    let cancel = handoff(&mut c, 1);
    assert!(matches!(
        decode(
            &cancel.bytes,
            context(Role::Host),
            &ProtocolLimits::ABSOLUTE
        )
        .unwrap()
        .body,
        Body::Cancel(_)
    ));
    cancel.permit.unwrap().check(at(2)).unwrap();
}

#[test]
fn ingress_queue_time_is_not_added_to_the_native_receive_lifetime() {
    let input = owner(10_000_000);
    let peer = owner(10_000_000);
    let mut source = session(&peer, Role::Controller);
    let mut target = session(&input, Role::Host);
    let mut sink = Platform::default();
    source.offer(1, "", None, at(0)).unwrap();
    let begin = handoff(&mut source, 0);
    let commit = handoff(&mut source, 0);
    target
        .receive_before(&begin.bytes, &mut sink, || at(2_999_999), at(3_000_000))
        .unwrap();
    assert_eq!(
        target.receive_before(&commit.bytes, &mut sink, || at(3_000_000), at(6_000_000)),
        Err(Error::Expired.into())
    );
    assert_eq!(sink.text, [] as [String; 0]);
    assert_eq!(target.retained_bytes(), 0);
    assert!(!target.is_closed());
    // Expiry never permits replay with a fresh first-record handoff deadline.
    assert_eq!(
        target.receive_before(&begin.bytes, &mut sink, || at(3_000_001), at(6_000_000)),
        Err(Error::Replay.into())
    );
}
