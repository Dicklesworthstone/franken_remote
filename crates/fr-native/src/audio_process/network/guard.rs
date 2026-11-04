//! Read-only projection of the original immutable output custody guards.
//! Publication and reads never retain a lock across a transport operation.
use super::{ConfigurationEgress, Cx, INFLIGHT_PACKETS, PendingEgress, now};
use fr_transport::quic::audio::RetirementRequest;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Default)]
pub(super) struct Snapshot {
    pub(super) inflight: [Option<PendingEgress>; INFLIGHT_PACKETS],
    pub(super) configuration: Option<ConfigurationEgress>,
    pub(super) control_until: Option<u64>,
    pub(super) stop_until: Option<u64>,
    pub(super) retired: bool,
}
struct Inner {
    cx: Cx,
    started: u64,
    retirement: RetirementRequest,
    snapshot: Mutex<Snapshot>,
    failed: AtomicBool,
}
#[derive(Clone)]
pub(super) struct Shared(Arc<Inner>);
impl Shared {
    pub(super) fn new(cx: Cx, retirement: RetirementRequest, started: u64) -> Self {
        Self(Arc::new(Inner {
            cx,
            started,
            retirement,
            snapshot: Mutex::new(Snapshot::default()),
            failed: AtomicBool::new(false),
        }))
    }
    pub(super) fn publish(&self, snapshot: Snapshot) {
        // The serialized owner publishes only BETWEEN q operations. A hostile
        // concurrent reader or poisoned lock fails closed, never blocks capture
        // or invents an empty snapshot to make progress.
        match self.0.snapshot.try_lock() {
            Ok(mut retained) => *retained = snapshot,
            Err(_) => self.0.failed.store(true, Ordering::Release),
        }
    }
    pub(super) fn permits(&self) -> bool {
        if self.0.failed.load(Ordering::Acquire) {
            return false;
        }
        let Ok(retained) = self.0.snapshot.try_lock() else {
            return false;
        };
        if retained.retired {
            // The sole writer sets this only after proving the datagrams
            // drained, or after irrevocably closing their original connection.
            return true;
        }
        let Ok(at) = now(&self.0.cx) else {
            return false;
        };
        if at < self.0.started
            || !retained
                .inflight
                .iter()
                .flatten()
                .all(|guard| guard.live(at))
        {
            return false;
        }
        if retained.control_until.is_some_and(|until| at >= until)
            || retained
                .configuration
                .as_ref()
                .is_some_and(|guard| !guard.is_open())
            || retained.stop_until.is_some_and(|until| at >= until)
        {
            // Reliable configuration/stop bytes can retire their exact pair at
            // the transport's checked boundary. Protected datagrams above
            // cannot be retracted this way and therefore refuse the connection.
            self.0.retirement.request();
        }
        true
    }
    pub(super) fn operation_check(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let shared = self.clone();
        Arc::new(move || shared.permits())
    }
}
