//! Minimal libpulse 17 stable ABI from pulse/{mainloop,context,stream,operation,
//! sample,def}.h. Only opaque pointers and these two public C-layout value types
//! cross the boundary. Native structs, callbacks and pointers never escape.
use std::ffi::{c_char, c_int, c_void};

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct SampleSpec {
    pub format: c_int,
    pub rate: u32,
    pub channels: u8,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct BufferAttr {
    pub maxlength: u32,
    pub tlength: u32,
    pub prebuf: u32,
    pub minreq: u32,
    pub fragsize: u32,
}
pub(super) type Success = Option<unsafe extern "C" fn(*mut c_void, c_int, *mut c_void)>;
#[cfg(target_endian = "little")]
pub(super) const S16_NATIVE: c_int = 3; // PA_SAMPLE_S16LE, not the ALSA enum.
#[cfg(target_endian = "big")]
pub(super) const S16_NATIVE: c_int = 4;
pub(super) const CONTEXT_READY: c_int = 4;
pub(super) const STREAM_READY: c_int = 2;
pub(super) const START_CORKED: c_int = 0x0001;
pub(super) const INTERPOLATE_TIMING: c_int = 0x0002;
pub(super) const DONT_MOVE: c_int = 0x0200;
pub(super) const ADJUST_LATENCY: c_int = 0x2000;
pub(super) const FAIL_ON_SUSPEND: c_int = 0x20000;
pub(super) const SEEK_ABSOLUTE: c_int = 1;

// System SONAME, not a caller-controlled dlopen path or bundled downloader.
// The installed local audio server and library are explicit native trust limits.
#[link(name = "libpulse.so.0", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    pub(super) fn pa_mainloop_new() -> *mut c_void;
    pub(super) fn pa_mainloop_free(mainloop: *mut c_void);
    pub(super) fn pa_mainloop_get_api(mainloop: *mut c_void) -> *mut c_void;
    pub(super) fn pa_mainloop_iterate(
        mainloop: *mut c_void,
        block: c_int,
        retval: *mut c_int,
    ) -> c_int;
    pub(super) fn pa_context_new(api: *mut c_void, name: *const c_char) -> *mut c_void;
    pub(super) fn pa_context_connect(
        context: *mut c_void,
        server: *const c_char,
        flags: c_int,
        api: *const c_void,
    ) -> c_int;
    pub(super) fn pa_context_get_state(context: *const c_void) -> c_int;
    pub(super) fn pa_context_disconnect(context: *mut c_void);
    pub(super) fn pa_context_unref(context: *mut c_void);
    pub(super) fn pa_stream_new(
        context: *mut c_void,
        name: *const c_char,
        spec: *const SampleSpec,
        map: *const c_void,
    ) -> *mut c_void;
    pub(super) fn pa_stream_connect_playback(
        stream: *mut c_void,
        device: *const c_char,
        attr: *const BufferAttr,
        flags: c_int,
        volume: *const c_void,
        sync: *mut c_void,
    ) -> c_int;
    pub(super) fn pa_stream_get_state(stream: *const c_void) -> c_int;
    pub(super) fn pa_stream_get_sample_spec(stream: *const c_void) -> *const SampleSpec;
    pub(super) fn pa_stream_get_buffer_attr(stream: *const c_void) -> *const BufferAttr;
    pub(super) fn pa_stream_get_device_index(stream: *const c_void) -> u32;
    pub(super) fn pa_stream_is_suspended(stream: *const c_void) -> c_int;
    pub(super) fn pa_stream_is_corked(stream: *const c_void) -> c_int;
    pub(super) fn pa_stream_cork(
        stream: *mut c_void,
        cork: c_int,
        callback: Success,
        userdata: *mut c_void,
    ) -> *mut c_void;
    pub(super) fn pa_stream_flush(
        stream: *mut c_void,
        callback: Success,
        userdata: *mut c_void,
    ) -> *mut c_void;
    pub(super) fn pa_stream_update_timing_info(
        stream: *mut c_void,
        callback: Success,
        userdata: *mut c_void,
    ) -> *mut c_void;
    pub(super) fn pa_stream_get_time(stream: *mut c_void, usec: *mut u64) -> c_int;
    pub(super) fn pa_stream_get_latency(
        stream: *mut c_void,
        usec: *mut u64,
        negative: *mut c_int,
    ) -> c_int;
    pub(super) fn pa_stream_writable_size(stream: *const c_void) -> usize;
    pub(super) fn pa_stream_write(
        stream: *mut c_void,
        data: *const c_void,
        bytes: usize,
        free: Option<unsafe extern "C" fn(*mut c_void)>,
        offset: i64,
        seek: c_int,
    ) -> c_int;
    pub(super) fn pa_stream_disconnect(stream: *mut c_void) -> c_int;
    pub(super) fn pa_stream_unref(stream: *mut c_void);
    pub(super) fn pa_operation_get_state(operation: *const c_void) -> c_int;
    pub(super) fn pa_operation_cancel(operation: *mut c_void);
    pub(super) fn pa_operation_unref(operation: *mut c_void);
}
