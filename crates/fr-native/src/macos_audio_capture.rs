//! One original macOS system-playback capture, isolated in the existing media
//! worker. `ScreenCaptureKit` supplies audio only; this owner has no microphone,
//! input, peer, display-pixel or permission-prompt capability. Its filter uses
//! one display as an API anchor, NOT as an audio privacy boundary.
//!
//! Native callbacks never enter Rust or retain a Rust pointer. They copy a
//! validated float32 sample buffer into fixed native storage, bounded in count
//! and samples. The safe packer converts only finite samples and emits at most
//! four frames. Every retained frame keeps this exact !Send owner and its
//! original sleep-inclusive expiry, including through libopus work.
use crate::{clock, macos_audio_capture_policy as policy};
use core::{ffi::c_void, ptr::NonNull};
use fr_core::audio::AudioChannels;
use fr_media::worker::audio::CaptureCounters;
use std::{cell::Cell, rc::Rc};

pub use policy::Error;
use policy::{MAX_CHUNK_VALUES, Packer, Stamp};

const OPEN_NS: u64 = 2_000_000_000;
const MAX_READ_CHUNKS: usize = 8;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NativeCounters {
    gaps: u32,
    overruns: u32,
}

unsafe extern "C" {
    fn fr_ac_open(channels: u32, out: *mut *mut c_void) -> i32;
    fn fr_ac_read(
        owner: *mut c_void,
        samples: *mut f32,
        capacity: usize,
        stamp: *mut Stamp,
        counters: *mut NativeCounters,
    ) -> i32;
    fn fr_ac_check(owner: *mut c_void) -> i32;
    /// Consumes the native ownership once. On uncertain destruction it retains
    /// a terminal native tombstone until the supervised process is reaped.
    fn fr_ac_close(owner: *mut c_void) -> i32;
}

fn status(code: i32) -> Result<(), Error> {
    Err(match code {
        0 => return Ok(()),
        -1 => Error::Configuration,
        -2 => Error::Unavailable,
        -3 => Error::Permission,
        -4 => Error::Allocation,
        -5 => Error::Clock,
        -6 => Error::Expired,
        -7 => Error::Closed,
        -8 => Error::Format,
        -9 => Error::BufferLimit,
        -11 => Error::Busy,
        -12 => Error::Cleanup,
        -13 => Error::Suspended,
        -14 => Error::Discontinuity,
        _ => Error::Native,
    })
}

struct Owner {
    raw: Cell<Option<NonNull<c_void>>>,
    first_error: Cell<Option<Error>>,
    closed: Cell<Option<Result<(), Error>>>,
    last_now: Cell<u64>,
}
impl Owner {
    fn fail(&self, error: Error) -> Error {
        let original = self.first_error.get().unwrap_or(error);
        self.first_error.set(Some(original));
        original
    }

    fn now(&self) -> Result<u64, Error> {
        let now = clock::monotonic_ns().ok_or_else(|| self.fail(Error::Clock))?;
        if now < self.last_now.get() {
            return Err(self.fail(Error::Clock));
        }
        self.last_now.set(now);
        Ok(now)
    }

    fn check(&self) -> Result<u64, Error> {
        if let Some(error) = self.first_error.get() {
            return Err(error);
        }
        self.now()?;
        let raw = self.raw.get().ok_or(Error::Closed)?;
        // SAFETY: Rc retains this exact native owner. Only this single-threaded
        // owner can close it, and no Rust callback/reentrant call enters here.
        status(unsafe { fr_ac_check(raw.as_ptr()) }).map_err(|error| self.fail(error))?;
        self.now()
    }

    fn close(&self) -> Result<(), Error> {
        if let Some(result) = self.closed.get() {
            return result;
        }
        // Retained Frames see the terminal state BEFORE any native stop/wait.
        self.fail(Error::Closed);
        let result = self.raw.take().map_or(Ok(()), |raw| {
            // SAFETY: the sole native ownership is taken exactly once. Native
            // close drains callbacks before releasing it, or retains its own
            // tombstone on failure; it never retains a Rust address.
            status(unsafe { fr_ac_close(raw.as_ptr()) })
        });
        self.closed.set(Some(result));
        result
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Exact source owner and original validity bound travel with every PCM frame.
/// Closing Source immediately invalidates its retained Frames, including a
/// frame held by an encoder caller. Payload bytes are never printed by Debug.
pub struct Frame {
    owner: Rc<Owner>,
    packed: policy::Packed,
}
impl Frame {
    pub fn samples(&self) -> &[i16] {
        &self.packed.samples
    }
    pub const fn timestamp_samples(&self) -> u64 {
        self.packed.timestamp_samples
    }
    pub fn check(&self) -> Result<(), Error> {
        let now = self.owner.check()?;
        if now >= self.packed.valid_until_ns {
            return Err(Error::Expired);
        }
        Ok(())
    }
}
impl core::fmt::Debug for Frame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MacAudioFrame")
            .field("values", &self.packed.samples.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct Batch {
    pub frames: Vec<Frame>,
    pub counters: CaptureCounters,
}

/// Unique caller-thread owner. Rc deliberately forbids moving native custody
/// or retained frames to another thread. `ScreenCaptureKit`'s callback queue
/// belongs entirely to the native owner and carries no Rust references.
pub struct Source {
    owner: Rc<Owner>,
    packer: Packer,
    scratch: Box<[f32]>,
    counters: NativeCounters,
}
impl Source {
    /// Complete the original, bounded native start. Actual sample formats are
    /// checked on every callback; framework acceptance is not format evidence.
    pub fn open(channels: AudioChannels, frame_samples: u16) -> Result<Self, Error> {
        let packer = Packer::new(channels.count(), frame_samples)?;
        let before = clock::monotonic_ns().ok_or(Error::Clock)?;
        let until = before.checked_add(OPEN_NS).ok_or(Error::Clock)?;
        let mut raw = core::ptr::null_mut();
        // SAFETY: writable output lives across the call. Native open installs
        // sole ownership only on success; every partial operation owns and
        // retires its original callback context, even after an expired return.
        status(unsafe { fr_ac_open(u32::from(channels.count()), &raw mut raw) })?;
        let owner = Rc::new(Owner {
            raw: Cell::new(Some(NonNull::new(raw).ok_or(Error::Allocation)?)),
            first_error: Cell::new(None),
            closed: Cell::new(None),
            last_now: Cell::new(before),
        });
        if owner.check()? >= until {
            return Err(owner.fail(Error::Expired));
        }
        Ok(Self {
            owner,
            packer,
            scratch: vec![0.0; MAX_CHUNK_VALUES].into_boxed_slice(),
            counters: NativeCounters::default(),
        })
    }

    /// Recheck the original source after ALL codec work and before publishing
    /// an encoded batch. A callback failure cannot be hidden by earlier output.
    pub fn check(&self) -> Result<(), Error> {
        self.owner.check().map(|_| ())
    }

    fn read_time(&self, until: u64) -> Result<u64, Error> {
        let now = self.owner.check()?;
        if now >= until {
            return Err(self.owner.fail(Error::Expired));
        }
        Ok(now)
    }

    /// At most eight bounded native pops and four output frames. Source age,
    /// this read's original budget and native terminal state are checked after
    /// every copy and before returning; a backlog never renews any one of them.
    pub fn pull(&mut self) -> Result<Batch, Error> {
        let result = self.pull_inner();
        if let Err(error) = result {
            self.packer.clear();
            self.scratch.fill(0.0);
            return Err(self.owner.fail(error));
        }
        result
    }

    fn pull_inner(&mut self) -> Result<Batch, Error> {
        let until = self
            .owner
            .check()?
            .checked_add(policy::READ_BUDGET_NS)
            .ok_or(Error::Clock)?;
        for _ in 0..MAX_READ_CHUNKS {
            self.read_time(until)?;
            let raw = self.owner.raw.get().ok_or(Error::Closed)?;
            let mut stamp = Stamp::default();
            let mut counters = NativeCounters::default();
            // SAFETY: the live exact owner and fixed writable float/scalar
            // buffers cover the complete declared capacity. Native copies no
            // more than that capacity and retains none of these pointers.
            let result = unsafe {
                fr_ac_read(
                    raw.as_ptr(),
                    self.scratch.as_mut_ptr(),
                    self.scratch.len(),
                    &raw mut stamp,
                    &raw mut counters,
                )
            };
            let now = self.read_time(until)?;
            if counters.gaps < self.counters.gaps || counters.overruns < self.counters.overruns {
                return Err(Error::Clock);
            }
            self.counters = counters;
            if result == 1 {
                break; // No ready audio is not invented silence.
            }
            status(result)?;
            let values = usize::try_from(stamp.frames)
                .ok()
                .and_then(|frames| frames.checked_mul(usize::try_from(stamp.channels).ok()?))
                .filter(|values| *values <= self.scratch.len())
                .ok_or(Error::BufferLimit)?;
            self.packer.push(stamp, &self.scratch[..values], now)?;
            self.scratch[..values].fill(0.0);
        }
        let now = self.read_time(until)?;
        let frames = self
            .packer
            .take(now)
            .into_iter()
            .map(|packed| Frame {
                owner: self.owner.clone(),
                packed,
            })
            .collect();
        Ok(Batch {
            frames,
            counters: CaptureCounters {
                gaps: self.counters.gaps.saturating_add(self.packer.gaps()),
                overruns: self.counters.overruns,
            },
        })
    }

    /// Stop admission first, then destroy the exact native stream and drain
    /// its callback queue. An uncertain close is an error, never Stopped proof.
    pub fn close(&mut self) -> Result<(), Error> {
        self.packer.clear();
        self.scratch.fill(0.0);
        self.owner.close()
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
