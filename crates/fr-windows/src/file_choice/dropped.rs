//! Affine custody of native file-drop storage, before any source preparation.
//! The platform supplies its one release operation; storage cannot be cloned
//! or retained after the bounded copy, including refusal and unwind paths.

pub struct DropMemory<T> {
    value: Option<T>,
    finish: fn(T),
}

impl<T> DropMemory<T> {
    pub const fn new(value: T, finish: fn(T)) -> Self {
        Self {
            value: Some(value),
            finish,
        }
    }

    /// The original storage is released before the result reaches its caller.
    /// The platform callback must return owned bytes, never a borrowed pointer.
    pub fn copy<R>(self, read: impl FnOnce(&T) -> R) -> R {
        read(self.value.as_ref().expect("original drop storage"))
    }
}

impl<T> Drop for DropMemory<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            (self.finish)(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        change_display::MenuChoice,
        file_choice::{Choice, DropMemory, Error},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn counted() -> (Arc<AtomicUsize>, DropMemory<Arc<AtomicUsize>>) {
        fn finish(count: Arc<AtomicUsize>) {
            assert_eq!(count.fetch_add(1, Ordering::SeqCst), 0);
            drop(count);
        }
        let count = Arc::new(AtomicUsize::new(0));
        let memory = DropMemory::new(Arc::clone(&count), finish);
        (count, memory)
    }

    #[test]
    fn declining_without_a_viewer_or_ready_lane_finishes_original_storage_once() {
        let (count, memory) = counted();
        drop(memory);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn copied_drop_storage_finishes_before_source_preparation_can_begin() {
        let (count, memory) = counted();
        let bytes = memory.copy(|owned| {
            assert_eq!(owned.load(Ordering::SeqCst), 0);
            vec![1, 2, 3]
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(bytes, [1, 2, 3]);
    }

    #[test]
    fn native_copy_refusal_and_unwind_still_finish_the_drop_once() {
        for refusal in [Error::InvalidPath, Error::Expired, Error::Cleanup] {
            let (count, memory) = counted();
            assert_eq!(memory.copy(|_| Err::<(), _>(refusal)), Err(refusal));
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
        let (count, memory) = counted();
        assert!(std::panic::catch_unwind(|| memory.copy(|_| panic!("copy failed"))).is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_drop_queued_before_rearming_cannot_spend_the_new_attempt() {
        let original = Arc::new(());
        // The HWND and Shared can survive an internal reconnect. Even a drop
        // only two milliseconds old must postdate the new armed snapshot.
        let mut new_arm = MenuChoice::new(Arc::clone(&original), 7, 12, 1_020);
        assert!(new_arm.take(&original, 7, 12, true, 1_019, 1_021).is_none());
        assert!(new_arm.take(&original, 7, 12, true, 1_030, 1_031).is_none());
    }

    #[test]
    fn original_drop_scope_cannot_be_replaced_while_the_event_is_queued() {
        let original = Arc::new(());
        for (lifetime, attempt, available) in [(8, 12, true), (7, 13, true), (7, 12, false)] {
            let mut arm = MenuChoice::new(Arc::clone(&original), 7, 12, 1_000);
            assert!(
                arm.take(&original, lifetime, attempt, available, 1_020, 1_021)
                    .is_none()
            );
            assert!(arm.take(&original, 7, 12, true, 1_030, 1_031).is_none());
        }
        let mut arm = MenuChoice::new(Arc::clone(&original), 7, 12, 1_000);
        assert!(arm.take(&Arc::new(()), 7, 12, true, 1_020, 1_021).is_none());
    }

    #[test]
    fn one_fresh_drop_keeps_native_queue_rounding_and_preparation_age() {
        let original = Arc::new(());
        let mut arm = MenuChoice::new(Arc::clone(&original), 7, 12, 1_000);
        let request = arm.take(&original, 7, 12, true, 1_020, 1_040).unwrap();
        assert_eq!(request.at, 1_004);
        assert!(arm.take(&original, 7, 12, true, 1_041, 1_042).is_none());
        let mut choice = Choice::from_gesture(request.at, 1_040).unwrap();
        choice.check(3_003).unwrap();
        assert_eq!(choice.finish(3_004), Err(Error::Expired));
    }

    #[test]
    fn drop_timestamps_keep_wrap_and_ambiguous_tick_refusal() {
        let original = Arc::new(());
        let wrap = 1_u64 << 32;
        let mut arm = MenuChoice::new(Arc::clone(&original), 7, 12, wrap - 20);
        let request = arm.take(&original, 7, 12, true, 5, wrap + 6).unwrap();
        assert_eq!(request.at, wrap - 11);
        for (message, now) in [(1_000, 1_001), (1_021, 1_020), (1_010, 1_094)] {
            let mut arm = MenuChoice::new(Arc::clone(&original), 7, 12, 1_000);
            assert!(arm.take(&original, 7, 12, true, message, now).is_none());
        }
    }
}
