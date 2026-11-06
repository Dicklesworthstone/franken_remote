//! One negotiated host operating-point snapshot on the original control lane.
//! No report is input, presentation, transport credit or freshness evidence.
use fr_core::limits::ProtocolLimits;
use fr_media::freshness::ReceiverLifetime;
use fr_transport::quic::{Disposition, Messages, Priority, Route, StreamRoute};
use fr_wire::{
    Kind, WireError,
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::{ControlBinding, Selection},
    quality_decision::{self as wire, Decision},
};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};

struct Snapshot {
    binding: Binding,
    decision: Decision,
    lifetime: Option<ReceiverLifetime>,
}
#[derive(Default)]
struct Shared {
    current: AtomicBool,
    value: Mutex<Option<Snapshot>>,
}

/// Read-only access to the original viewer's one bounded host report. This
/// retains neither the viewer nor its report owner, and grants no input rights.
#[derive(Clone)]
pub struct Handle {
    shared: Weak<Shared>,
    control: super::StreamingViewerControl,
}
impl Handle {
    /// Unknown during recovery, after cancellation/EOF/drop, before a report,
    /// or while the bounded snapshot is being updated. No network I/O occurs.
    pub fn current(&self) -> Option<Decision> {
        let shared = self.shared.upgrade()?;
        if self.control.is_stopped() || !shared.current.load(Ordering::Acquire) {
            return None;
        }
        let snapshot = shared.value.try_lock().ok()?;
        let snapshot = snapshot.as_ref()?;
        (snapshot.lifetime.as_ref()?.is_live()
            && shared.current.load(Ordering::Acquire)
            && !self.control.is_stopped())
        .then_some(snapshot.decision)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    NotSelected,
    Binding,
    Sequence,
    Wire(WireError),
    Closed,
}

#[derive(Clone, Copy)]
enum Phase {
    Active,
    Recovering(Binding),
    Closed,
}

/// A fixed-size receiver: one snapshot, one sequence floor, no history queue.
pub(super) struct ViewerQuality {
    binding: Binding,
    inbound: StreamRoute,
    limits: ProtocolLimits,
    phase: Phase,
    sequence: u64,
    /// Fully validated records, including old-view reports retired in recovery.
    pub(super) received: u64,
    shared: Arc<Shared>,
    lifetime: Option<ReceiverLifetime>,
}

pub(super) fn is_quality(bytes: &[u8]) -> bool {
    bytes.get(6..8) == Some(&(Kind::QualityDecision as u16).to_be_bytes())
}

impl ViewerQuality {
    pub(super) fn attach(
        selection: &Selection,
        parent: ControlBinding,
        view: Binding,
        inbound: StreamRoute,
    ) -> Result<Option<Self>, Fault> {
        if !selection
            .capabilities
            .iter()
            .any(|c| c.name == wire::CAPABILITY)
        {
            return Ok(None);
        }
        let binding = checked_binding(selection, parent, view, inbound)?;
        Ok(Some(Self {
            binding,
            inbound,
            limits: selection.limits,
            phase: Phase::Active,
            sequence: 0,
            received: 0,
            shared: Arc::default(),
            lifetime: None,
        }))
    }

    pub(super) fn set_lifetime(&mut self, lifetime: ReceiverLifetime) {
        self.shared.current.store(false, Ordering::Release);
        self.lifetime = Some(lifetime);
    }

    pub(super) fn handle(&self, control: super::StreamingViewerControl) -> Handle {
        Handle {
            shared: Arc::downgrade(&self.shared),
            control,
        }
    }

    pub(super) fn last(&self) -> Option<Decision> {
        if matches!(self.phase, Phase::Recovering(_)) {
            return None;
        }
        self.shared
            .value
            .try_lock()
            .ok()?
            .as_ref()
            .filter(|snapshot| snapshot.binding == self.binding)
            .map(|snapshot| snapshot.decision)
    }

    pub(super) fn current(&self) -> Option<Decision> {
        (matches!(self.phase, Phase::Active) && self.shared.current.load(Ordering::Acquire))
            .then_some(self.last())
            .flatten()
    }

    pub(super) fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, Fault> {
        if matches!(self.phase, Phase::Closed) {
            return Err(Fault::Closed);
        }
        if route != Route::Stream(self.inbound) {
            return Err(Fault::Binding);
        }
        let decode = |binding| {
            wire::decode(
                bytes,
                binding,
                &self.limits,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
        };
        let decision = match decode(self.binding) {
            Ok(decision) => decision,
            Err(WireError::InvalidBinding) if matches!(self.phase, Phase::Recovering(_)) => {
                let Phase::Recovering(next) = self.phase else {
                    unreachable!("checked phase")
                };
                let next = decode(next).map_err(Fault::Wire)?;
                if next.sequence <= self.sequence {
                    return Err(Fault::Sequence);
                }
                // Keep the exact bytes in their original bounded transport slot.
                // They cannot describe the current view before replacement ends.
                return Ok(Disposition::Blocked);
            }
            Err(error) => return Err(Fault::Wire(error)),
        };
        if decision.sequence <= self.sequence {
            return Err(Fault::Sequence);
        }
        if matches!(self.phase, Phase::Active) {
            // Retain the exact record in the existing transport slot if a
            // reader is copying the prior point. The host only sends changes,
            // so silently losing this update could leave stable state unknown.
            let Ok(mut value) = self.shared.value.try_lock() else {
                return Ok(Disposition::Blocked);
            };
            *value = Some(Snapshot {
                binding: self.binding,
                decision,
                lifetime: self.lifetime.clone(),
            });
            self.shared.current.store(true, Ordering::Release);
        }
        self.sequence = decision.sequence;
        self.received = self.received.saturating_add(1);
        // An obsolete report is consumed only after full validation against
        // its original private scope, including its sequence and wire grammar.
        Ok(Disposition::Consumed)
    }

    pub(super) fn begin_recovery(&mut self) -> Result<(), Fault> {
        match self.phase {
            Phase::Active => {
                let mut next = self.binding;
                next.recovery = next.recovery.next().ok_or(Fault::Binding)?;
                self.phase = Phase::Recovering(next);
                self.shared.current.store(false, Ordering::Release);
                Ok(())
            }
            Phase::Recovering(_) => Ok(()),
            Phase::Closed => Err(Fault::Closed),
        }
    }

    pub(super) fn rebind(
        &mut self,
        selection: &Selection,
        parent: ControlBinding,
        view: Binding,
        inbound: StreamRoute,
    ) -> Result<(), Fault> {
        let binding = checked_binding(selection, parent, view, inbound)?;
        if !matches!(self.phase, Phase::Recovering(next) if next == binding)
            || inbound != self.inbound
            || selection.limits != self.limits
        {
            return Err(Fault::Binding);
        }
        self.binding = binding;
        self.phase = Phase::Active;
        self.shared.current.store(false, Ordering::Release);
        // Sequence and received count belong to the original parent, not the
        // replaced media. Recovery must not admit a replay or invent a report.
        Ok(())
    }

    pub(super) fn close(&mut self) {
        self.phase = Phase::Closed;
        self.shared.current.store(false, Ordering::Release);
        // Retain the one historical diagnostic, never expose it as current.
    }
}

fn checked_binding(
    selection: &Selection,
    parent: ControlBinding,
    mut view: Binding,
    inbound: StreamRoute,
) -> Result<Binding, Fault> {
    if !wire::selected(selection) {
        return Err(Fault::NotSelected);
    }
    if parent.host_boot != view.parent.host_boot
        || parent.os_session != view.parent.os_session
        || parent.remote_session != view.parent.remote_session
        || inbound.binding != parent.id
        || inbound.messages != Messages::SessionControl
        || inbound.priority != Priority::Critical
        || inbound.outbound
    {
        return Err(Fault::Binding);
    }
    if inbound.maximum < wire::RECORD_BYTES
        || selection.limits.max_control_message_bytes() < wire::RECORD_BYTES as u32
    {
        return Err(Fault::Wire(WireError::ResourceLimit));
    }
    view.validate().map_err(Fault::Wire)?;
    view.parent = parent;
    view.validate().map_err(Fault::Wire)?;
    Ok(view)
}

#[cfg(test)]
mod tests;
