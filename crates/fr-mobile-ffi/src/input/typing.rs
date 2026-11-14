//! Bounded custody of original software-keyboard callbacks. A retry retains
//! the same capture and stroke index; neither a new key nor a new owner can
//! substitute for a busy original. No clocks, authority or OS calls live here.
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const MAX_BATCHES: usize = 64;
pub(crate) const MAX_STROKES: u32 = 4096;
const MAX_TRANSITIONS: usize = 18;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Physical key content: deliberately no diagnostic formatting.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stroke {
    pub usage: u16,
    pub modifiers: u8,
}

#[derive(Clone, Copy)]
pub(crate) struct Transition {
    pub usage: u16,
    pub pressed: bool,
}

/// One complete stroke releases only modifiers that it presses. The producer
/// reserves capacity for this whole plan before submitting its first event.
pub(crate) struct Plan {
    events: [Transition; MAX_TRANSITIONS],
    len: usize,
}

impl Plan {
    pub(crate) fn new(stroke: Stroke, held: &[bool; 256]) -> Self {
        let mut plan = Self {
            events: [Transition {
                usage: 0,
                pressed: false,
            }; MAX_TRANSITIONS],
            len: 0,
        };
        if held[usize::from(stroke.usage)] {
            return plan;
        }
        for bit in 0..8_u16 {
            let usage = 0xE0 + bit;
            if stroke.modifiers & (1 << bit) != 0
                && usage != stroke.usage
                && !held[usize::from(usage)]
            {
                plan.push(usage, true);
            }
        }
        let modifiers = plan.len;
        plan.push(stroke.usage, true);
        plan.push(stroke.usage, false);
        for index in (0..modifiers).rev() {
            plan.push(plan.events[index].usage, false);
        }
        plan
    }

    fn push(&mut self, usage: u16, pressed: bool) {
        self.events[self.len] = Transition { usage, pressed };
        self.len += 1;
    }

    pub(crate) fn events(&self) -> &[Transition] {
        &self.events[..self.len]
    }
}

pub(crate) struct Batch<C> {
    pub id: u64,
    pub capture: C,
    next: u32,
    retry: Option<Stroke>,
}

impl<C> Batch<C> {
    /// A busy stroke may be attempted again only at its original index and
    /// with its original content. Successful indices can never be replayed.
    pub(crate) fn begin(&mut self, index: u32, stroke: Stroke) -> bool {
        if index != self.next
            || index >= MAX_STROKES
            || self.retry.is_some_and(|original| original != stroke)
        {
            return false;
        }
        self.retry = Some(stroke);
        true
    }

    pub(crate) fn accepted(&mut self) {
        self.next += 1;
        self.retry = None;
    }
}

pub(crate) struct Batches<C> {
    entries: Vec<Batch<C>>,
}

impl<C> Default for Batches<C> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<C> Batches<C> {
    pub(crate) fn insert(&mut self, capture: C) -> Option<u64> {
        if self.entries.len() >= MAX_BATCHES || self.entries.try_reserve(1).is_err() {
            return None;
        }
        // Process-wide, monotonic identities prevent a token for a retired
        // source/session from matching another source's numerically equal slot.
        let id = NEXT_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .ok()?;
        self.entries.push(Batch {
            id,
            capture,
            next: 0,
            retry: None,
        });
        Some(id)
    }

    pub(crate) fn take(&mut self, id: u64) -> Option<Batch<C>> {
        let index = self.entries.iter().position(|batch| batch.id == id)?;
        Some(self.entries.swap_remove(index))
    }

    /// Restore the one entry taken while its original Granted lock is held.
    /// No other insert can run between take and restore.
    pub(crate) fn restore(&mut self, batch: Batch<C>) {
        self.entries.push(batch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(usage: u16, modifiers: u8) -> Stroke {
        Stroke { usage, modifiers }
    }

    fn values(plan: &Plan) -> Vec<(u16, bool)> {
        plan.events()
            .iter()
            .map(|event| (event.usage, event.pressed))
            .collect()
    }

    #[test]
    fn a_stroke_owns_only_its_new_modifiers_and_releases_in_reverse_order() {
        let mut held = [false; 256];
        held[0xE1] = true;
        assert_eq!(
            values(&Plan::new(stroke(4, 0b0000_1011), &held)),
            [
                (0xE0, true),
                (0xE3, true),
                (4, true),
                (4, false),
                (0xE3, false),
                (0xE0, false)
            ]
        );
        held[4] = true;
        assert!(Plan::new(stroke(4, 0xff), &held).events().is_empty());
        assert!(
            held[0xE1] && held[4],
            "planning never changes physical ownership"
        );
    }

    #[test]
    fn every_modifier_combination_fits_the_complete_fixed_plan() {
        for modifiers in 0..=u8::MAX {
            let plan = Plan::new(stroke(4, modifiers), &[false; 256]);
            assert_eq!(plan.events().len(), 2 + 2 * modifiers.count_ones() as usize);
            assert!(plan.events().len() <= MAX_TRANSITIONS);
            let modifier_key = Plan::new(stroke(0xE1, modifiers), &[false; 256]);
            assert_eq!(
                modifier_key
                    .events()
                    .iter()
                    .filter(|event| event.usage == 0xE1)
                    .count(),
                2
            );
        }
    }

    #[test]
    fn busy_retries_preserve_content_and_consumed_indices_never_repeat() {
        let mut batches = Batches::default();
        let id = batches.insert("original capture").unwrap();
        let mut batch = batches.take(id).unwrap();
        assert!(batch.begin(0, stroke(4, 1)));
        assert!(batch.begin(0, stroke(4, 1)));
        assert!(!batch.begin(0, stroke(5, 1)));
        assert!(!batch.begin(1, stroke(4, 1)));
        assert_eq!(batch.capture, "original capture");
        batch.accepted();
        assert!(!batch.begin(0, stroke(4, 1)));
        assert!(batch.begin(1, stroke(5, 0)));
        batches.restore(batch);
        assert_eq!(batches.take(id).unwrap().next, 1);
    }

    #[test]
    fn completed_batch_bound_does_not_wrap_or_accept_a_new_suffix() {
        let mut batches = Batches::default();
        let id = batches.insert(()).unwrap();
        let mut batch = batches.take(id).unwrap();
        for index in 0..MAX_STROKES {
            assert!(batch.begin(index, stroke(4, 0)));
            batch.accepted();
        }
        assert!(!batch.begin(MAX_STROKES, stroke(4, 0)));
        assert!(!batch.begin(0, stroke(4, 0)));
    }

    #[test]
    fn callback_slots_are_bounded_and_tokens_never_rebind_across_sources() {
        let mut original = Batches::default();
        let ids: Vec<_> = (0..MAX_BATCHES)
            .map(|_| original.insert(()).unwrap())
            .collect();
        assert!(original.insert(()).is_none());
        let mut replacement = Batches::default();
        for id in &ids {
            assert!(replacement.take(*id).is_none());
        }
        let token = replacement.insert(()).unwrap();
        assert!(!ids.contains(&token));
        let retired = ids[0];
        let _retired = original.take(retired).unwrap();
        assert!(original.take(retired).is_none());
        assert_ne!(original.insert(()).unwrap(), retired);
    }
}
