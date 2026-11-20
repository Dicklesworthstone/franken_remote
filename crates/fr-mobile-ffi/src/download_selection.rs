//! Bounded, one-use local destination custody before a native session starts.
//! Reserving precedes filesystem work, so concurrent callers cannot turn slow
//! directory checks into unbounded open descriptors or preparation workers.
#![forbid(unsafe_code)]

pub const MAX_DESTINATIONS: usize = 4;

struct Slot<T> {
    token: u64,
    value: Option<T>,
}

pub struct Destinations<T> {
    next: u64,
    slots: Vec<Slot<T>>,
}

impl<T> Default for Destinations<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Destinations<T> {
    pub const fn new() -> Self {
        Self {
            next: 1,
            slots: Vec::new(),
        }
    }

    pub fn reserve(&mut self) -> Option<u64> {
        if self.slots.len() >= MAX_DESTINATIONS {
            return None;
        }
        let token = self.next;
        self.next = self.next.checked_add(1)?;
        self.slots.push(Slot { token, value: None });
        Some(token)
    }

    pub fn prepared(&mut self, token: u64, value: T) -> bool {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.token == token) else {
            return false;
        };
        if slot.value.is_some() {
            return false;
        }
        slot.value = Some(value);
        true
    }

    /// Consume the exact prepared owner once. An unfinished reservation does
    /// not become a ready destination merely because a caller guessed its ID.
    pub fn take(&mut self, token: u64) -> Option<T> {
        let index = self
            .slots
            .iter()
            .position(|slot| slot.token == token && slot.value.is_some())?;
        self.slots.swap_remove(index).value
    }

    pub fn discard(&mut self, token: u64) -> bool {
        let Some(index) = self.slots.iter().position(|slot| slot.token == token) else {
            return false;
        };
        self.slots.swap_remove(index);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn pending_and_ready_destinations_share_one_resource_bound() {
        let mut owners = Destinations::new();
        let tokens: Vec<_> = (0..MAX_DESTINATIONS)
            .map(|_| owners.reserve().unwrap())
            .collect();
        assert!(owners.prepared(tokens[1], "selected"));
        assert!(owners.reserve().is_none());
        assert!(owners.take(tokens[0]).is_none());
        assert_eq!(owners.take(tokens[1]), Some("selected"));
        assert!(owners.take(tokens[1]).is_none());
        let next = owners.reserve().unwrap();
        assert!(next > *tokens.last().unwrap());
        assert!(owners.reserve().is_none());
    }

    #[test]
    fn a_cancelled_or_consumed_token_never_names_a_later_destination() {
        let mut owners = Destinations::new();
        let cancelled = owners.reserve().unwrap();
        assert!(owners.discard(cancelled));
        assert!(!owners.prepared(cancelled, "late filesystem result"));
        let current = owners.reserve().unwrap();
        assert_ne!(cancelled, current);
        assert!(owners.prepared(current, "original"));
        assert!(!owners.prepared(current, "replacement"));
        assert!(!owners.discard(cancelled));
        assert_eq!(owners.take(current), Some("original"));
        assert!(owners.take(current).is_none());
        assert!(!owners.discard(current));
    }

    #[test]
    fn cancelling_or_consuming_transfers_exact_original_resource_custody() {
        struct Resource(Arc<AtomicUsize>);
        impl Drop for Resource {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let released = Arc::new(AtomicUsize::new(0));
        let mut owners = Destinations::new();
        let token = owners.reserve().unwrap();
        assert!(owners.prepared(token, Resource(Arc::clone(&released))));
        let original = owners.take(token).unwrap();
        assert_eq!(released.load(Ordering::SeqCst), 0);
        drop(original);
        assert_eq!(released.load(Ordering::SeqCst), 1);
        let cancelled = owners.reserve().unwrap();
        assert!(owners.prepared(cancelled, Resource(Arc::clone(&released))));
        assert!(owners.discard(cancelled));
        assert_eq!(released.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn exhausted_identity_space_never_wraps_or_reuses_an_owner() {
        let mut owners = Destinations::<()>::new();
        owners.next = u64::MAX;
        assert_eq!(owners.reserve(), None);
        assert_eq!(owners.reserve(), None);
        assert!(owners.slots.is_empty());
        assert!(!owners.discard(0));
    }
}
