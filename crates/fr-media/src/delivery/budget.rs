//! Reservations follow compressed pictures out of the reassembly queue.
use super::DeliveryError;
use core::fmt;
use fr_core::limits::ProtocolLimits;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetUsage {
    pub bytes: usize,
    pub pictures: usize,
}
#[derive(Debug)]
struct Ledger {
    used: BudgetUsage,
    max_bytes: usize,
    max_pictures: usize,
}
/// Clone the ledger, not its allowances. Reuse it across recovery generations
/// so retained decoder inputs from a closing generation remain charged.
#[derive(Debug, Clone)]
pub struct MediaBudget(Arc<Mutex<Ledger>>);
impl MediaBudget {
    pub fn new(limits: &ProtocolLimits) -> Result<Self, DeliveryError> {
        let max_bytes = usize::try_from(limits.per_viewer_compressed_bytes())
            .map_err(|_| DeliveryError::ResourceLimit)?;
        Ok(Self(Arc::new(Mutex::new(Ledger {
            used: BudgetUsage::default(),
            max_bytes,
            max_pictures: usize::from(limits.reassembly_window_pictures()),
        }))))
    }
    pub fn usage(&self) -> BudgetUsage {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .used
    }
    pub(crate) fn fits(&self, limits: &ProtocolLimits) -> bool {
        let ledger = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        u64::try_from(ledger.max_bytes).is_ok_and(|n| n <= limits.per_viewer_compressed_bytes())
            && ledger.max_pictures <= usize::from(limits.reassembly_window_pictures())
    }
    /// Check an individual allocation before inspecting aggregate slot pressure.
    /// Otherwise a full queue could disguise an impossible picture as recoverable.
    /// This takes no credit; allocate still reserves atomically and charges actual
    /// allocator capacity before publishing a buffer.
    pub(crate) fn check_allocation(
        &self,
        length: usize,
        metadata: usize,
    ) -> Result<(), DeliveryError> {
        let bytes = length
            .checked_add(metadata)
            .ok_or(DeliveryError::ResourceLimit)?;
        let ledger = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if bytes > ledger.max_bytes {
            return Err(DeliveryError::ResourceLimit);
        }
        Ok(())
    }
    fn reserve(&self, bytes: usize) -> Result<Reservation, DeliveryError> {
        let mut ledger = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A picture which cannot fit even in an empty ledger is not temporary
        // pressure. Never turn it into an IDR loop by releasing other pictures.
        if bytes > ledger.max_bytes {
            return Err(DeliveryError::ResourceLimit);
        }
        let next = ledger
            .used
            .bytes
            .checked_add(bytes)
            .ok_or(DeliveryError::ResourceLimit)?;
        if next > ledger.max_bytes || ledger.used.pictures >= ledger.max_pictures {
            return Err(DeliveryError::ReferenceCapacityExceeded);
        }
        ledger.used.bytes = next;
        ledger.used.pictures += 1;
        Ok(Reservation {
            ledger: self.clone(),
            bytes,
        })
    }
    pub(crate) fn allocate(
        &self,
        length: usize,
        metadata: usize,
    ) -> Result<TrackedBytes, DeliveryError> {
        let mut permit = self.reserve(
            length
                .checked_add(metadata)
                .ok_or(DeliveryError::ResourceLimit)?,
        )?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| DeliveryError::AllocationFailed)?;
        // Allocators may return more capacity than requested. Charge the actual
        // vector capacity before initializing or publishing the buffer.
        permit.grow(bytes.capacity() - length)?;
        bytes.resize(length, 0);
        Ok(TrackedBytes { bytes, permit })
    }
}
struct Reservation {
    ledger: MediaBudget,
    bytes: usize,
}
impl Reservation {
    fn grow(&mut self, extra: usize) -> Result<(), DeliveryError> {
        let mut ledger = self
            .ledger
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Apply the same distinction to allocator/metadata capacity growth.
        // Neither a refused growth nor its subsequent Drop may refund bytes
        // owned by another picture (including an old decoder generation).
        let own = self
            .bytes
            .checked_add(extra)
            .ok_or(DeliveryError::ResourceLimit)?;
        if own > ledger.max_bytes {
            return Err(DeliveryError::ResourceLimit);
        }
        let next = ledger
            .used
            .bytes
            .checked_add(extra)
            .ok_or(DeliveryError::ResourceLimit)?;
        if next > ledger.max_bytes {
            return Err(DeliveryError::ReferenceCapacityExceeded);
        }
        self.bytes = own;
        ledger.used.bytes = next;
        Ok(())
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut ledger = self
            .ledger
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ledger.used.bytes -= self.bytes;
        ledger.used.pictures -= 1;
    }
}
pub(crate) struct TrackedBytes {
    pub(crate) bytes: Vec<u8>,
    permit: Reservation,
}
impl TrackedBytes {
    pub(crate) fn charge_extra(&mut self, extra: usize) -> Result<(), DeliveryError> {
        self.permit.grow(extra)
    }
    pub(crate) fn belongs_to(&self, budget: &MediaBudget) -> bool {
        Arc::ptr_eq(&self.permit.ledger.0, &budget.0)
    }
}
impl fmt::Debug for TrackedBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrackedBytes")
            .field("length", &self.bytes.len())
            .field("charged_bytes", &self.permit.bytes)
            .finish()
    }
}

#[cfg(test)]
mod capacity_tests;
