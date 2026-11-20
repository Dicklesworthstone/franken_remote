//! Refuse a round that would make either unchanged destination unscannable.
#![forbid(unsafe_code)]

use super::{FolderManifest, SyncError, SyncLimits, SyncPlan, TransferLeg};

struct Destination {
    entries: u32,
    bytes: u64,
}

impl Destination {
    fn check(&self, limits: SyncLimits) -> Result<(), SyncError> {
        if self.entries > limits.max_entries {
            return Err(SyncError::EntryLimitExceeded);
        }
        if self.bytes > limits.max_total_bytes {
            return Err(SyncError::TotalBytesLimitExceeded);
        }
        Ok(())
    }

    fn publish(&mut self, bytes: u64, limits: SyncLimits) -> Result<(), SyncError> {
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or(SyncError::EntryLimitExceeded)?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(SyncError::TotalBytesLimitExceeded)?;
        self.check(limits)
    }
}

/// Simulate the entire plan before its first publication. `KeepBoth` always
/// creates a new entry, including when the original name is occupied. Private
/// staging entries consumed scan work too and remain charged to the next scan.
pub(super) fn validate(
    plan: &SyncPlan,
    host: &FolderManifest,
    controller: &FolderManifest,
    limits: SyncLimits,
) -> Result<(), SyncError> {
    let mut host = Destination {
        entries: host.inspected_entries,
        bytes: host.total_bytes,
    };
    let mut controller = Destination {
        entries: controller.inspected_entries,
        bytes: controller.total_bytes,
    };
    host.check(limits)?;
    controller.check(limits)?;
    for transfer in &plan.transfers {
        let destination = match transfer.leg {
            TransferLeg::HostToController => &mut controller,
            TransferLeg::ControllerToHost => &mut host,
        };
        destination.publish(transfer.file.size, limits)?;
    }
    Ok(())
}
