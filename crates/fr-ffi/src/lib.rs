//! `fr-ffi`: Pinned `FFmpeg` hardware codec wrapper behind the `fr-media` contracts.
//!
//! # Architectural Boundaries (AGENTS.md §3.2)
//!
//! This crate is a named, audited FFI boundary crate wrapping the `FFmpeg` C ABI
//! (`libavcodec`, `libavutil`, `libswscale`). It implements the safe contracts
//! defined in `fr-media`:
//! - [`fr_media::codec::Encoder`]
//! - [`fr_media::codec::Decoder`]
//! - [`fr_media::codec::DecodedPicture`]
//! - [`fr_media::surface::GpuSurface`]
//!
//! ### Pointer Provenance & Ownership
//! All native `FFmpeg` pointers (`AVCodecContext`, `AVFrame`, `AVPacket`, `SwsContext`)
//! are strictly owned by their respective wrapper structs (`FfmpegEncoder`, `FfmpegDecoder`).
//! Pointers are checked against null upon allocation and zeroed on free.
//!
//! ### Buffer Padding Contract
//! The H.265 / HEVC bitstream specification and `FFmpeg` decoding API require that
//! input packet buffers have at least `AV_INPUT_BUFFER_PADDING_SIZE` (64 bytes)
//! of allocated, zeroed trailing padding beyond the declared payload length.
//! `fr-ffi` enforces this by allocating decoder input buffers via `FFmpeg`'s
//! internal packet allocator (`av_new_packet`), ensuring unpadded network slices
//! are never directly submitted to native decoding functions.
//!
//! ### Shutdown Order
//! To avoid driver deadlocks, memory leaks, or use-after-free conditions:
//! 1. In-flight output and input frames/packets are unreferenced and freed first.
//! 2. Pixel format scaling contexts (`SwsContext`) are freed.
//! 3. Codec contexts (`AVCodecContext`) are closed and freed.
//! 4. Hardware frame pools and device references (`AVBufferRef`) are unreferenced.
//! 5. Outer heap memory is freed.
//!
//! ### Thread Confinement
//! Codec contexts and FFI shims are strictly thread-confined: `FfmpegEncoder` and
//! `FfmpegDecoder` implement `Send` to allow ownership transfer across worker
//! threads, but intentionally do NOT implement `Sync`. They must execute in a
//! dedicated supervised media worker process, strictly isolated from the broker
//! and authority event loops.

extern crate alloc;

pub mod decoder;
pub mod encoder;
pub mod error;
pub mod surface;

pub use decoder::FfmpegDecoder;
pub use encoder::{FfiEncoderBackend, FfmpegEncoder};
pub use surface::{FfiDecodedPicture, FfiSurface};
