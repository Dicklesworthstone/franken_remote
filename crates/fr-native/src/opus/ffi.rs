//! Minimal stable libopus C ABI. Constants and signatures are from Xiph's
//! `opus.h` / `opus_defines.h` (1.5); no native structs or Rust layouts cross the ABI.
use std::ffi::{c_int, c_void};

pub(super) const APPLICATION_AUDIO: c_int = 2049;
pub(super) const SET_BITRATE: c_int = 4002;
pub(super) const SET_COMPLEXITY: c_int = 4010;
pub(super) const GET_LOOKAHEAD: c_int = 4027;

// Linux's installed, versioned ABI; no dlopen, path search API, build downloader,
// FFmpeg dependency or alternate runtime. Package provenance is a deployment gate.
#[link(name = "libopus.so.0", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    pub(super) fn opus_encoder_get_size(channels: c_int) -> c_int;
    pub(super) fn opus_encoder_create(
        rate: i32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut c_void;
    pub(super) fn opus_encoder_destroy(state: *mut c_void);
    pub(super) fn opus_encoder_ctl(state: *mut c_void, request: c_int, ...) -> c_int;
    pub(super) fn opus_encode(
        state: *mut c_void,
        pcm: *const i16,
        frame_size: c_int,
        packet: *mut u8,
        max_bytes: i32,
    ) -> i32;
}

pub(super) const RESET_STATE: c_int = 4028;

#[link(name = "libopus.so.0", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    pub(super) fn opus_decoder_get_size(channels: c_int) -> c_int;
    pub(super) fn opus_decoder_create(rate: i32, channels: c_int, error: *mut c_int)
    -> *mut c_void;
    pub(super) fn opus_decoder_destroy(state: *mut c_void);
    pub(super) fn opus_decoder_ctl(state: *mut c_void, request: c_int, ...) -> c_int;
    pub(super) fn opus_decode(
        state: *mut c_void,
        packet: *const u8,
        len: i32,
        pcm: *mut i16,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    pub(super) fn opus_packet_get_nb_samples(packet: *const u8, len: i32, rate: i32) -> c_int;
    pub(super) fn opus_packet_parse(
        packet: *const u8,
        len: i32,
        toc: *mut u8,
        frames: *mut *const u8,
        sizes: *mut i16,
        payload_offset: *mut c_int,
    ) -> c_int;
}
