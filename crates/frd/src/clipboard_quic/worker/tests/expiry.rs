//! Expiry of one complete unsubmitted ingress record is item-local. These
//! deterministic cases use the production worker and original wire/core owner.
use super::*;
use fr_core::clipboard::Error as ClipboardError;
use fr_wire::clipboard::session::SessionError;

fn incoming_record(sequence: u64, body: Body<'_>, incoming: Context) -> Vec<u8> {
    let mut bytes = vec![0; 1024];
    let length = fr_wire::clipboard::encode(
        fr_wire::clipboard::Message {
            stamp: Stamp {
                id: 100 + u128::from(sequence),
                source: fr_core::clipboard::Endpoint::Controller,
                sequence,
            },
            body,
        },
        incoming,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(length);
    bytes
}
fn begin() -> Body<'static> {
    Body::Begin {
        total_bytes: 3,
        chunks: 1,
    }
}
fn incoming() -> Context {
    Context {
        sender: Role::Controller,
        ..context()
    }
}
impl Fixture {
    fn queue(&self, sequence: u64, body: Body<'_>) {
        let bytes = incoming_record(sequence, body, incoming());
        let local = shared::now(&self.worker.cx).unwrap();
        assert_eq!(
            self.shared
                .accept(&bytes, local, HostInstant::from_micros(local)),
            Ok(super::super::super::Disposition::Consumed)
        );
    }
    fn collect(&self) -> Option<Received> {
        let mut inbox = self.shared.inbox.lock().unwrap();
        let result = inbox.result.take();
        if result.is_some() {
            assert!(inbox.busy);
            assert_eq!(self.shared.inbound_until.load(Ordering::Acquire), 0);
            inbox.busy = false;
        }
        result
    }
    fn receive(&mut self, sequence: u64, body: Body<'_>) -> Option<Received> {
        self.queue(sequence, body);
        self.step().unwrap();
        self.collect()
    }
    fn fresh_copy(&mut self) {
        assert_eq!(self.receive(2, begin()), None);
        assert_eq!(
            self.receive(
                2,
                Body::Chunk {
                    index: 0,
                    offset: 0,
                    bytes: b"new",
                },
            ),
            None
        );
        assert!(matches!(
            self.receive(2, Body::Commit { total_bytes: 3 }),
            Some(Received::Consumed(Some(fr_core::clipboard::Receipt {
                publication: Publication::SubmittedToOs,
                ..
            })))
        ));
    }
}

#[test]
fn queued_and_explicitly_deferred_items_expire_without_replacing_the_original_worker() {
    for deferred in [false, true] {
        let mut f = Fixture::new(false);
        f.step().unwrap();
        f.os.lock().unwrap().text = Some("existing local clipboard".into());
        f.queue(1, begin());
        if deferred {
            f.os.lock().unwrap().unsettled = true;
            f.step().unwrap();
            assert!(f.shared.inbox.lock().unwrap().item.is_some());
            assert!(f.shared.inbox.lock().unwrap().result.is_none());
        }
        f.permission.paused.store(true, Ordering::Release);
        f.clock.advance_to(Time::from_millis(2_999));
        f.step().unwrap();
        let deadline = f.shared.inbound_until.load(Ordering::Acquire);
        assert_eq!(deadline, 3_000_000);
        assert!(f.shared.inbox.lock().unwrap().item.is_some());
        let changes = f.os.lock().unwrap().changes;
        f.clock.advance_to(Time::from_millis(3_000));
        f.step().unwrap();
        assert!(f.shared.gate.is_open());
        {
            let inbox = f.shared.inbox.lock().unwrap();
            assert!(inbox.busy);
            assert!(inbox.item.is_none());
            assert!(inbox.expired.is_none());
            assert_eq!(
                inbox.result,
                Some(Received::Refused(SessionError::Clipboard(
                    ClipboardError::Expired
                )))
            );
        }
        {
            let os = f.os.lock().unwrap();
            assert_eq!(os.changes, changes, "expiry invoked native changes");
            assert_eq!(os.text.as_deref(), Some("existing local clipboard"));
            assert_eq!(
                (os.reads, os.publications, os.suspends, os.closes),
                (0, 0, 0, 0)
            );
        }
        let fresh = incoming_record(2, begin(), incoming());
        assert_eq!(
            f.shared
                .accept(&fresh, deadline, HostInstant::from_micros(deadline)),
            Ok(super::super::super::Disposition::Blocked),
            "the uncollected refusal must retain the same single busy slot"
        );
        assert!(f.collect().is_some());
        f.permission.paused.store(false, Ordering::Release);
        f.os.lock().unwrap().unsettled = false;
        assert_eq!(
            f.receive(1, begin()),
            Some(Received::Refused(SessionError::Clipboard(
                ClipboardError::Replay
            )))
        );
        f.fresh_copy();
        let os = f.os.lock().unwrap();
        assert_eq!(
            (
                os.opens,
                os.watches,
                os.publications,
                os.suspends,
                os.closes
            ),
            (1, 1, 1, 0, 0)
        );
        assert!(f.shared.gate.is_open());
    }
}

#[test]
fn queued_chunk_and_commit_expiry_release_only_the_old_transfer() {
    for commit in [false, true] {
        let mut f = Fixture::new(false);
        assert_eq!(f.receive(1, begin()), None);
        let chunk = Body::Chunk {
            index: 0,
            offset: 0,
            bytes: b"old",
        };
        if commit {
            assert_eq!(f.receive(1, chunk), None);
        }
        f.queue(
            1,
            if commit {
                Body::Commit { total_bytes: 3 }
            } else {
                chunk
            },
        );
        f.permission.paused.store(true, Ordering::Release);
        f.clock.advance_to(Time::from_millis(3_000));
        f.step().unwrap();
        assert_eq!(
            f.collect(),
            Some(Received::Refused(SessionError::Clipboard(
                ClipboardError::UnknownTransfer
            )))
        );
        assert_eq!(f.os.lock().unwrap().publications, 0);
        assert!(f.shared.gate.is_open());
        f.permission.paused.store(false, Ordering::Release);
        f.fresh_copy();
        assert_eq!(f.os.lock().unwrap().publications, 1);
        assert_eq!(f.os.lock().unwrap().watches, 1);
    }
}

#[test]
fn either_original_bound_can_expire_the_unsubmitted_slot_without_refreshing_the_other() {
    for host_first in [false, true] {
        let f = Fixture::new(false);
        f.queue(1, begin());
        let original = f.shared.inbound_until.load(Ordering::Acquire);
        let bound = f.shared.inbound_bound.load(Ordering::Acquire);
        let (local, host) = if host_first {
            (original - 1, HostInstant::from_micros(bound))
        } else {
            (original, HostInstant::from_micros(bound - 1))
        };
        f.shared.expire_incoming(local, host).unwrap();
        let inbox = f.shared.inbox.lock().unwrap();
        assert!(inbox.item.is_none());
        assert!(inbox.expired.is_some());
        assert!(inbox.busy);
        assert_eq!(f.shared.inbound_bound.load(Ordering::Acquire), bound);
        assert_eq!(f.shared.inbound_until.load(Ordering::Acquire), original);
    }
}

#[test]
fn contended_inbox_does_not_hide_expiry_on_either_original_clock() {
    for host_first in [false, true] {
        let f = Fixture::new(false);
        f.queue(1, begin());
        let local = f.shared.inbound_until.load(Ordering::Acquire);
        let host = f.shared.inbound_bound.load(Ordering::Acquire);
        let _inbox = f.shared.inbox.lock().unwrap();
        assert_eq!(
            f.shared
                .expire_incoming(local - 1, HostInstant::from_micros(host - 1)),
            Ok(())
        );
        let (local, host) = if host_first {
            (local - 1, HostInstant::from_micros(host))
        } else {
            (local, HostInstant::from_micros(host - 1))
        };
        assert_eq!(
            f.shared.expire_incoming(local, host),
            Err(Error::HandoffExpired)
        );
    }
}

#[test]
fn native_receive_ownership_has_no_safe_expiry_exemption_on_either_clock() {
    for host_first in [false, true] {
        let mut f = Fixture::new(false);
        f.queue(1, begin());
        // This is the exact shared state while receive_before owns the bytes:
        // absence of a result proves nothing about preparation or OS effect.
        let _executing = f.shared.inbox.lock().unwrap().item.take().unwrap();
        let until = f.shared.inbound_until.load(Ordering::Acquire);
        let (local, host) = if host_first {
            (until - 1, HostInstant::from_micros(until))
        } else {
            (until, HostInstant::from_micros(until - 1))
        };
        assert_eq!(
            f.worker.check_handoffs(local, host),
            Err(Error::HandoffExpired)
        );
        f.clock.advance_to(Time::from_millis(3_000));
        assert_eq!(f.step(), Err(Error::HandoffExpired));
        assert!(!f.shared.gate.is_open());
        assert_eq!(f.os.lock().unwrap().publications, 0);
    }
}

#[test]
fn foreign_binding_and_switch_generation_changes_remain_terminal_at_ingress_expiry() {
    for foreign in [false, true] {
        let mut f = Fixture::new(false);
        let context = Context {
            channel: if foreign {
                context().channel + 1
            } else {
                context().channel
            },
            ..incoming()
        };
        let bytes = incoming_record(1, begin(), context);
        assert_eq!(
            f.shared.accept(&bytes, 0, HostInstant::ORIGIN),
            Ok(super::super::super::Disposition::Consumed)
        );
        f.permission.paused.store(true, Ordering::Release);
        if !foreign {
            let (local, _) = f.shared.gate.switches();
            local.set_enabled(false);
            local.set_enabled(true);
        }
        f.clock.advance_to(Time::from_millis(3_000));
        let expected = if foreign {
            Error::Session(SessionError::Wire(fr_wire::WireError::InvalidBinding))
        } else {
            Error::SwitchChanged
        };
        assert_eq!(f.step(), Err(expected));
        assert!(!f.shared.gate.is_open());
        assert_eq!(f.os.lock().unwrap().publications, 0);
    }
}

#[test]
fn actual_publication_receipts_survive_a_late_collector_and_paused_worker() {
    for publication in [Publication::SubmittedToOs, Publication::UnknownEffect] {
        let mut f = Fixture::new(false);
        f.os.lock().unwrap().publication = Some(publication);
        assert_eq!(f.receive(1, begin()), None);
        assert_eq!(
            f.receive(
                1,
                Body::Chunk {
                    index: 0,
                    offset: 0,
                    bytes: b"old",
                },
            ),
            None
        );
        f.queue(1, Body::Commit { total_bytes: 3 });
        f.step().unwrap();
        assert_eq!(f.shared.inbound_until.load(Ordering::Acquire), 0);
        f.permission.paused.store(true, Ordering::Release);
        f.clock.advance_to(Time::from_millis(3_000));
        f.step().unwrap();
        let Some(Received::Consumed(Some(receipt))) = f.collect() else {
            panic!("an actual native receipt was reclassified or discarded")
        };
        assert_eq!(receipt.publication, publication);
        assert_eq!(f.os.lock().unwrap().publications, 1);
        assert!(f.shared.gate.is_open());
    }
}
