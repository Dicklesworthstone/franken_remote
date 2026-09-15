//! ABI-only connection boundary. C copies every input before returning and
//! allocates/frees only its opaque connection and native event/reply objects.
use core::ffi::{c_char, c_int, c_void};
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub(super) struct Atoms {
    pub clipboard: u32,
    pub utf8: u32,
    pub targets: u32,
    pub timestamp: u32,
    pub incr: u32,
    pub clock: u32,
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub(super) struct Event {
    pub kind: u32,
    pub window: u32,
    pub property: u32,
    pub target: u32,
    pub time: u32,
    pub selection: u32,
    pub sequence: u32,
}
unsafe extern "C" {
    pub(super) fn fr_clip_open(
        display: *const c_char,
        atoms: *mut Atoms,
        window: *mut u32,
    ) -> *mut c_void;
    pub(super) fn fr_clip_close(handle: *mut c_void);
    pub(super) fn fr_clip_tick(handle: *mut c_void, sequence: *mut u32) -> c_int;
    pub(super) fn fr_clip_owner(handle: *mut c_void, owner: *mut u32) -> c_int;
    pub(super) fn fr_clip_publish(handle: *mut c_void, time: u32) -> c_int;
    pub(super) fn fr_clip_watch(handle: *mut c_void, window: u32, enabled: c_int) -> c_int;
    pub(super) fn fr_clip_property(
        handle: *mut c_void,
        window: u32,
        property: u32,
        kind: u32,
        format: u8,
        count: u32,
        bytes: *const c_void,
    ) -> c_int;
    pub(super) fn fr_clip_notify(handle: *mut c_void, event: *const Event, property: u32) -> c_int;
    pub(super) fn fr_clip_next(handle: *mut c_void, event: *mut Event) -> c_int;
}
