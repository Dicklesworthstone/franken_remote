//! One OS-share seat: native cleanup ownership and local consent exclusion.
//! Neither inhibition nor its release grants input or revives an old lease.
use super::{Control, Error, StopReason};
use std::sync::{
    Arc, Mutex, MutexGuard, RwLock,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

const MAX_INHIBITIONS: u8 = 8;

#[derive(Default)]
struct Admission {
    epoch: u64,
    inhibitors: u8,
}
#[derive(Default)]
struct State {
    occupied: AtomicBool,
    retired: AtomicU8,
    admission: Mutex<Admission>,
    // Writers exist only before a native owner is launched, or after its
    // control is fenced AND native destruction has finished. A live owner
    // therefore cannot hide behind a writer. Retirement and inhibition only
    // take concurrent read guards, clone the control, then stop it unlocked.
    // In particular, admission contention never delays an entered child's fence.
    active: RwLock<Option<Control>>,
}
impl State {
    fn lock(&self) -> MutexGuard<'_, Admission> {
        // No native work, caller callbacks or fallible allocation under this lock.
        self.admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Share the SAME seat across every controller and approval surface of an OS
/// share. A fresh seat must never bypass unresolved cleanup on an existing one.
#[derive(Clone, Default)]
pub struct Seat(Arc<State>);
impl Seat {
    /// Exact local ownership identity, not an OS/session number or permission.
    pub fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub fn is_occupied(&self) -> bool {
        self.0.occupied.load(Ordering::Acquire)
    }

    /// Permanently end this exact OS-share seat. The first reason is latched
    /// before any lock; pending reservations and all future grants are refused.
    /// The current authority and native process fence run without the admission
    /// lock or a mutable owner lock. Cleanup still belongs to the original
    /// Driver; retirement never clears occupancy or certifies a held-key release.
    pub fn retire(&self, reason: StopReason) {
        let _ =
            self.0
                .retired
                .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire);
        self.stop_active(self.retirement_reason().unwrap_or(reason));
    }

    /// The original terminal cause. Consent guard removal, completed native
    /// cleanup and a later monitor epoch cannot reopen this seat.
    pub fn retirement_reason(&self) -> Option<StopReason> {
        StopReason::from_raw(self.0.retired.load(Ordering::Acquire))
    }

    fn stop_active(&self, reason: StopReason) {
        let active = self
            .0
            .active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(control) = active {
            control.stop(reason);
        }
    }

    /// Block new native owners and fence the current lease synchronously. No
    /// native call or cleanup wait runs here. The original Driver must keep
    /// draining; `Inhibition::is_ready` becomes true only after its existing
    /// cleanup/destructor rules release the seat. Pending reservations are also
    /// invalidated, even if the barrier is removed before they try to start.
    ///
    /// This is negative authority, not a pause/resume operation. Removing the
    /// last guard permits only a NEW reservation and fresh grant. Keep the guard
    /// through the native lifetime of the protected consent surface, not merely
    /// until its decision is made. At most eight overlapping barriers may exist.
    pub fn inhibit(&self) -> Result<Inhibition, Error> {
        if self.retirement_reason().is_some() {
            return Err(Error::SeatInhibited);
        }
        {
            let mut admission = self.0.lock();
            if admission.inhibitors == MAX_INHIBITIONS || self.retirement_reason().is_some() {
                return Err(Error::SeatInhibited);
            }
            let epoch = admission.epoch.checked_add(1).ok_or(Error::SeatInhibited)?;
            admission.epoch = epoch;
            admission.inhibitors += 1;
        }
        // Own the count before invoking the stop hook so unwinding cannot leak
        // it. Never invoke a caller-installed executor fence under the seat lock.
        let guard = Inhibition(self.clone());
        self.stop_active(self.retirement_reason().unwrap_or(StopReason::Suspended));
        Ok(guard)
    }

    pub(crate) fn reserve(&self) -> Result<SeatReservation, Error> {
        if self.retirement_reason().is_some() {
            return Err(Error::SeatInhibited);
        }
        let admission = self.0.lock();
        if admission.inhibitors != 0 || self.retirement_reason().is_some() {
            return Err(Error::SeatInhibited);
        }
        self.0
            .occupied
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::SeatBusy)?;
        Ok(SeatReservation {
            seat: self.clone(),
            epoch: admission.epoch,
            owned: true,
        })
    }

    pub(super) fn install(&self, epoch: u64, control: &Control) -> Result<(), Error> {
        if let Some(reason) = self.retirement_reason() {
            control.stop(reason);
            return Err(Error::SeatInhibited);
        }
        let installed = {
            let admission = self.0.lock();
            if admission.inhibitors != 0
                || admission.epoch != epoch
                || self.retirement_reason().is_some()
            {
                Err(Error::SeatInhibited)
            } else {
                // This reservation owns the occupied seat, but no native thread
                // has started. Retirement may safely wait for this writer.
                let mut active = self
                    .0
                    .active
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if active.is_some() {
                    Err(Error::SeatBusy)
                } else {
                    *active = Some(control.clone());
                    Ok(())
                }
            }
        };
        // Publishing before this check closes registration versus retirement:
        // either retire sees this control, or this path fences it before launch.
        if let Some(reason) = self.retirement_reason() {
            control.stop(reason);
            return Err(Error::SeatInhibited);
        }
        installed
    }

    pub(super) fn release(&self) {
        let old = {
            let _admission = self.0.lock();
            // A failed pre-launch reservation, or the original finalizer AFTER
            // fencing, cleanup and native destruction. Never an executing owner.
            let old = self
                .0
                .active
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            // Independent from inhibition AND retirement: cleanup can complete
            // without permitting another owner. Only original custody calls.
            self.0.occupied.store(false, Ordering::Release);
            old
        };
        drop(old);
    }
}

/// A non-cloneable local barrier. Never removes another guard's exclusion and
/// never clears native cleanup uncertainty. Drop only after native UI teardown.
#[must_use = "retain until the protected native consent surface is destroyed"]
pub struct Inhibition(Seat);
impl Inhibition {
    /// True means this seat's old native owner is gone and NEW input remains
    /// excluded by this guard. It says nothing about rolling back prior effects.
    pub fn is_ready(&self) -> bool {
        !self.0.is_occupied()
    }
}
impl std::fmt::Debug for Inhibition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputInhibition")
            .field("ready", &self.is_ready())
            .finish_non_exhaustive()
    }
}
impl Drop for Inhibition {
    fn drop(&mut self) {
        self.0.0.lock().inhibitors -= 1;
    }
}

/// Before launch, Drop releases the reservation. After launch only the native
/// finalizer releases it, after cleanup AND destructor completion. The epoch
/// also fences a broker that was preparing a grant when local consent started.
pub(crate) struct SeatReservation {
    pub(super) seat: Seat,
    pub(super) epoch: u64,
    pub(super) owned: bool,
}
impl Drop for SeatReservation {
    fn drop(&mut self) {
        if self.owned {
            self.seat.release();
        }
    }
}

#[cfg(test)]
mod tests;
