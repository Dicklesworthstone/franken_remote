//! Thread-local Rust allocation observation, not a foreign-code allocation count.
//! This wraps the real allocator; no codec operation or native result is faked.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
thread_local! {
    static COUNT: Cell<Option<usize>> = const { Cell::new(None) };
}
struct Allocator;
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
fn record() {
    let _ = COUNT.try_with(|count| {
        if let Some(n) = count.get() {
            count.set(Some(n + 1));
        }
    });
}
// SAFETY: every operation delegates with the original pointer/layout to System.
// Observation uses only const-initialized thread-local Cells, never allocation.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: unchanged valid layout from the allocator caller.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: unchanged valid layout from the allocator caller.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: matching System allocation and unchanged original layout.
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record();
        // SAFETY: matching System allocation, layout and requested new size.
        unsafe { System.realloc(pointer, layout, size) }
    }
}
struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        COUNT.with(|count| count.set(None));
    }
}
pub fn during<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    COUNT.with(|count| assert_eq!(count.replace(Some(0)), None));
    let guard = Guard;
    let result = operation();
    let count = COUNT.with(|count| count.get().unwrap());
    drop(guard);
    (result, count)
}
