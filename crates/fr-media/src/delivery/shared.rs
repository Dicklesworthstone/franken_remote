//! Physical compressed storage for an admitted shared encoder (plan 11.2/12.3).
//! One pool follows retained frames across subscribers and recovery generations.
use super::{BudgetUsage, DeliveryError};
use crate::access_unit::{EncodedAccessUnit, FrameId, FrameKind};
use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_wire::{FrameDescriptor, PipelineState, Progress, SourceObservation, WireError};
use std::sync::{Arc, Mutex};

const MAX_PICTURES: usize = 64;
struct Ledger {
    used: BudgetUsage,
    maximum: BudgetUsage,
}
/// Clone the physical ledger, never its allowance. Keep this pool on the OS
/// share-session/source owner, not on an individual viewer. The deliberately
/// conservative pool ceiling is no larger than the configured compressed-media
/// ceiling, independent of the number of subscribers. Multiple encoders may use
/// the SAME ledger when the host needs an aggregate bound.
#[derive(Clone)]
pub struct SharedFramePool {
    ledger: Arc<Mutex<Ledger>>,
    limits: ProtocolLimits,
}
impl core::fmt::Debug for SharedFramePool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SharedFramePool")
            .field("usage", &self.usage())
            .finish_non_exhaustive()
    }
}
impl SharedFramePool {
    pub fn new(
        limits: ProtocolLimits,
        maximum_bytes: usize,
        maximum_pictures: usize,
    ) -> Result<Self, DeliveryError> {
        if maximum_bytes <= Allocation::METADATA_BYTES
            || !u64::try_from(maximum_bytes)
                .is_ok_and(|n| n <= limits.per_viewer_compressed_bytes())
            || !(1..=MAX_PICTURES).contains(&maximum_pictures)
        {
            return Err(DeliveryError::InvalidPolicy);
        }
        Ok(Self {
            ledger: Arc::new(Mutex::new(Ledger {
                used: BudgetUsage::default(),
                maximum: BudgetUsage {
                    bytes: maximum_bytes,
                    pictures: maximum_pictures,
                },
            })),
            limits,
        })
    }
    pub fn usage(&self) -> BudgetUsage {
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .used
    }
    /// Non-reserving producer credit, including actual Vec capacity and shared
    /// metadata. The single capture owner must not issue another production job
    /// before transferring the first result. Concurrent producers need their own
    /// admission serialization; this snapshot is not an allocation permit.
    pub fn can_share_capacity(&self, capacity: usize) -> bool {
        let Ok(ledger) = self.ledger.lock() else {
            return false;
        };
        capacity
            .checked_add(Allocation::METADATA_BYTES)
            .and_then(|charge| ledger.used.bytes.checked_add(charge))
            .is_some_and(|total| {
                total <= ledger.maximum.bytes && ledger.used.pictures < ledger.maximum.pictures
            })
    }
    /// Move an already bounded encoder output without copying its Vec. Upstream
    /// production must itself be bounded BEFORE allocating/encoding; this is the
    /// persistent shared-storage admission, not permission for an unlimited input.
    /// Failed admission releases this output and never changes the ledger.
    pub fn share(&self, unit: EncodedAccessUnit) -> Result<SharedFrame, DeliveryError> {
        self.limits
            .validate_access_unit_len(unit.bytes().len())
            .map_err(|_| DeliveryError::ResourceLimit)?;
        let frame = unit.frame();
        let kind = unit.kind();
        let configuration = unit.config_generation();
        let capture_micros = unit.capture_micros();
        let bytes = unit.into_bytes();
        let charged = bytes
            .capacity()
            .checked_add(Allocation::METADATA_BYTES)
            .ok_or(DeliveryError::ResourceLimit)?;
        let mut ledger = self.ledger.lock().map_err(|_| DeliveryError::WrongState)?;
        let total = ledger
            .used
            .bytes
            .checked_add(charged)
            .ok_or(DeliveryError::ResourceLimit)?;
        if total > ledger.maximum.bytes || ledger.used.pictures >= ledger.maximum.pictures {
            return Err(DeliveryError::ResourceLimit);
        }
        ledger.used.bytes = total;
        ledger.used.pictures += 1;
        drop(ledger);
        Ok(SharedFrame(Arc::new(Allocation {
            frame,
            kind,
            configuration,
            capture_micros,
            bytes,
            permit: SharedPermit {
                pool: self.clone(),
                charged,
            },
        })))
    }
}
struct Allocation {
    // Drop the actual bytes BEFORE releasing their reservation.
    frame: FrameId,
    kind: FrameKind,
    configuration: CodecConfigurationGeneration,
    capture_micros: u64,
    bytes: Vec<u8>,
    permit: SharedPermit,
}
struct SharedPermit {
    pool: SharedFramePool,
    charged: usize,
}
impl Allocation {
    // Payload metadata plus Arc's strong/weak reference counters. Allocator
    // bookkeeping is not estimated here, just as Vec capacity excludes it.
    const METADATA_BYTES: usize = core::mem::size_of::<Self>() + 2 * core::mem::size_of::<usize>();
}
impl Drop for SharedPermit {
    fn drop(&mut self) {
        let mut ledger = self
            .pool
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ledger.used.bytes -= self.charged;
        ledger.used.pictures -= 1;
    }
}
/// An immutable encoded allocation. Cloning only increments its reference count;
/// the physical charge is released only when the LAST publisher/cache owner drops.
/// Recovery epochs are per subscription, not a mutable property of this buffer.
#[derive(Clone)]
pub struct SharedFrame(Arc<Allocation>);
impl core::fmt::Debug for SharedFrame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SharedFrame")
            .field("frame", &self.frame())
            .field("byte_len", &self.bytes().len())
            .field("charged_bytes", &self.allocation_charge())
            .finish_non_exhaustive()
    }
}
impl SharedFrame {
    pub fn bytes(&self) -> &[u8] {
        &self.0.bytes
    }
    pub fn frame(&self) -> FrameId {
        self.0.frame
    }
    pub fn kind(&self) -> FrameKind {
        self.0.kind
    }
    pub fn configuration(&self) -> CodecConfigurationGeneration {
        self.0.configuration
    }
    pub fn capture_micros(&self) -> u64 {
        self.0.capture_micros
    }
    pub fn allocation_charge(&self) -> usize {
        self.0.permit.charged
    }
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn progress(&self, stride: u32) -> Result<Progress, WireError> {
        Ok(Progress {
            descriptor: FrameDescriptor {
                frame: self.frame().as_raw(),
                total_bytes: u32::try_from(self.bytes().len())
                    .map_err(|_| WireError::ResourceLimit)?,
                stride,
                capture_micros: self.capture_micros(),
                reference: match self.kind() {
                    FrameKind::Idr { .. } => None,
                    FrameKind::Predicted { references } => Some(references.as_raw()),
                },
            },
            observed_micros: self.capture_micros(),
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        })
    }
}
