//! Reconcile already-sent custody after a local scheduling pause.
//! No application bytes leave this path and no application record is dispatched.

use super::{Error, QuicRecords, lifetime, now};
use asupersync::{cx::Cx, time::sleep};
use std::{pin::pin, time::Duration};

const RECEIVE_WAIT: Duration = Duration::from_millis(1);
const RECONCILE_BOUND: Duration = Duration::from_millis(10);
const RECONCILE_BOUND_US: u64 = 10_000;

impl QuicRecords {
    /// Before ordinary synchronous maintenance, reconcile an overdue fully
    /// staged send with ONE bounded authenticated receive batch. The native
    /// driver acknowledges incoming packets but never transmits application
    /// payload, retransmissions, or probes. Actual acknowledged stream offsets
    /// release custody; the unchanged deadline checks judge everything left.
    ///
    /// This can distinguish already-acknowledged bytes from a sender's own late
    /// scheduling turn. It does not establish when an ACK reached the socket,
    /// renew a delivery deadline, dispatch input, or establish fresh authority.
    /// Expired unsent/partial records and unsettled native I/O cannot use it.
    ///
    /// With no overdue send, this is a no-op: no I/O, authority callback, or
    /// mutation. It is NOT a substitute for `tick`/`send`/`receive` authority checks.
    /// Call it before those synchronous checks when servicing a session after
    /// a pause. `drive` also invokes it before ordinary payload work.
    pub async fn reconcile_acknowledgements(
        &mut self,
        cx: &Cx,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let result = match self.prepare_acknowledgement_reconciliation(cx, &mut authorize) {
            Ok(None) => return Ok(()),
            Err(error) => Err(error),
            Ok(Some(current)) => {
                let turn = pin!(self.read_acknowledgements(cx, current, &mut authorize));
                let bound = pin!(sleep(cx.now(), RECONCILE_BOUND));
                lifetime::within_turn_bound(turn, bound).await
            }
        };
        if let Err(error) = result {
            self.close_on_error(error);
        }
        result
    }

    // Type-erase synchronous policy work, as check() does. The async I/O
    // callback stays generic so existing Send and non-Send callers keep their
    // contracts; erasing it to dyn FnMut would make every drive non-Send.
    fn prepare_acknowledgement_reconciliation(
        &mut self,
        cx: &Cx,
        authorize: &mut dyn FnMut() -> bool,
    ) -> Result<Option<u64>, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let observed = now(cx)?;
        if !self
            .senders
            .iter()
            .any(|sender| sender.until.is_some_and(|until| observed >= until))
        {
            return Ok(None);
        }
        // This is a fixed observation-order exception for send custody only.
        // All other checks still run, including optional receive expiry and
        // explicit local/peer retirement. Those causes cannot be rescued by ACKs.
        let current = self.check_with_send_deadlines(cx, authorize, false)?;
        if self.native_io_unsettled
            || self.pending_writes.iter().any(|p| current >= p.deliver_by)
            || !self
                .senders
                .iter()
                .any(|sender| sender.until.is_some_and(|until| current >= until))
        {
            return self.check(cx, authorize).map(|_| None);
        }
        Ok(Some(current))
    }

    async fn read_acknowledgements(
        &mut self,
        cx: &Cx,
        current: u64,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let until = current
            .checked_add(RECONCILE_BOUND_US)
            .ok_or(Error::Clock)?;
        let lifetime = self.lifetime_check.clone();
        let operation = self.operation_check.clone();
        let gate = || {
            lifetime.as_ref().is_none_or(|check| check())
                && operation.as_ref().is_none_or(|check| check())
        };
        let mut held = self.hold()?;
        let (native, boundary) = held.io_parts();
        let result = {
            // Zero wait would flush ACKs without reading the socket. One
            // positive, bounded wait consumes at most the native 32-packet
            // batch; no loop can wait for a new ACK or postpone expiry again.
            let receive = pin!(native.acknowledge_io_once(cx, RECEIVE_WAIT));
            lifetime::poll_io_at_boundary(
                cx,
                Some(&gate),
                authorize,
                boundary,
                current,
                Some(until),
                receive,
            )
            .await
        };
        let native = held.release();
        // Entry rejected unsettled protected output. The ACK-only driver cannot
        // create protected application packets, so successful completion proves
        // that its own output has settled without flushing queued payload.
        self.finish_native_io(native, result)?;
        self.retire_acknowledged()?;
        self.check(cx, authorize)?;
        self.check_native_state()
    }
}
