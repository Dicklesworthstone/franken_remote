//! One locally authorized, endpoint-wide Windows playback source. The default
//! render endpoint is resolved once; changes or native failures end this owner.
//! It captures no microphone and never follows a replacement endpoint. This
//! boundary runs only in the supervised media worker, after audio admission.
//!
//! Every frame retains this exact thread-confined owner and its first native
//! QPC deadline through encoding and publication. Native calls may hang; the
//! parent supervisor owns that failure, not an unbounded application retry.
use crate::{host_audio_capture_policy as packing, windows_audio_capture_policy as policy};
use fr_core::audio::AudioChannels;
use fr_media::worker::audio::CaptureCounters;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

mod native;

pub use packing::Error;

const OPEN_NS: u64 = 2_000_000_000;
const MAX_READ_CHUNKS: usize = 8;

struct Owner {
    native: RefCell<Option<native::Capture>>,
    clock: native::Clock,
    last_now: Cell<u64>,
    first_error: Cell<Option<Error>>,
    closed: Cell<Option<Result<(), Error>>>,
}

impl Owner {
    fn fail(&self, error: Error) -> Error {
        let original = self.first_error.get().unwrap_or(error);
        self.first_error.set(Some(original));
        original
    }

    fn now(&self) -> Result<u64, Error> {
        let now = self.clock.now().map_err(|error| self.fail(error))?;
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
        self.native
            .borrow()
            .as_ref()
            .ok_or(Error::Closed)?
            .check()
            .map_err(|error| self.fail(error))?;
        // Device queries can block. The whole batch uses this later clock,
        // never a pre-native sample that would hide the time spent checking.
        self.now()
    }

    fn close(&self) -> Result<(), Error> {
        if let Some(result) = self.closed.get() {
            return result;
        }
        self.fail(Error::Closed);
        let result = self
            .native
            .borrow_mut()
            .take()
            .map_or(Ok(()), native::Capture::close);
        self.closed.set(Some(result));
        result
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// PCM and an immutable source-age bound. Closing the source immediately
/// invalidates retained frames, even when a caller already encoded one.
pub struct Frame {
    owner: Rc<Owner>,
    packed: packing::Packed,
}

impl Frame {
    pub fn samples(&self) -> &[i16] {
        &self.packed.samples
    }

    pub const fn timestamp_samples(&self) -> u64 {
        self.packed.timestamp_samples
    }

    pub fn check(&self) -> Result<(), Error> {
        if self.owner.check()? >= self.packed.valid_until_ns {
            return Err(Error::Expired);
        }
        Ok(())
    }
}

impl core::fmt::Debug for Frame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WindowsAudioFrame")
            .field("values", &self.packed.samples.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct Batch {
    pub frames: Vec<Frame>,
    pub counters: CaptureCounters,
}

/// Unique, caller-thread capture. Rc keeps COM custody and every frame on the
/// original thread. The only PCM retained is one partial plus four ready frames.
pub struct Source {
    owner: Rc<Owner>,
    channels: u8,
    timeline: policy::Timeline,
    packer: packing::Packer,
    scratch: Box<[i16]>,
}

impl Source {
    /// Request a real shared-mode loopback stream. The Windows audio engine
    /// converts its actual mix format to 48 kHz signed-16 mono/stereo; refusal
    /// remains unavailable, without a format guess or substitute input source.
    pub fn open(channels: AudioChannels, frame_samples: u16) -> Result<Self, Error> {
        let packer = packing::Packer::new_exact(channels.count(), frame_samples)?;
        let clock = native::Clock::new()?;
        let before = clock.now()?;
        let until = before.checked_add(OPEN_NS).ok_or(Error::Clock)?;
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(packing::MAX_CHUNK_VALUES)
            .map_err(|_| Error::Allocation)?;
        scratch.resize(packing::MAX_CHUNK_VALUES, 0);
        let source = native::Capture::open(channels.count())?;
        let owner = Rc::new(Owner {
            native: RefCell::new(Some(source)),
            clock,
            last_now: Cell::new(before),
            first_error: Cell::new(None),
            closed: Cell::new(None),
        });
        if owner.check()? >= until {
            return Err(owner.fail(Error::Expired));
        }
        Ok(Self {
            owner,
            channels: channels.count(),
            timeline: policy::Timeline::default(),
            packer,
            scratch: scratch.into_boxed_slice(),
        })
    }

    /// Verify original endpoint identity and all original deadlines against
    /// one clock sample taken after the final native call.
    pub fn check_frames(
        &self,
        frames: &[Frame],
    ) -> Result<[bool; packing::MAX_BATCH_FRAMES], Error> {
        if frames.len() > packing::MAX_BATCH_FRAMES {
            return Err(Error::BufferLimit);
        }
        if frames
            .iter()
            .any(|frame| !Rc::ptr_eq(&frame.owner, &self.owner))
        {
            return Err(Error::Configuration);
        }
        let now = self.owner.check()?;
        let mut live = [false; packing::MAX_BATCH_FRAMES];
        for (index, frame) in frames.iter().enumerate() {
            live[index] = now < frame.packed.valid_until_ns;
        }
        Ok(live)
    }

    fn read_time(&self, until: u64) -> Result<u64, Error> {
        let now = self.owner.check()?;
        if now >= until {
            return Err(self.owner.fail(Error::Expired));
        }
        Ok(now)
    }

    /// Poll up to eight bounded native packets without waiting for sound.
    /// Silence is emitted only when WASAPI marks an actual packet SILENT.
    pub fn pull(&mut self) -> Result<Batch, Error> {
        let result = self.pull_inner();
        if let Err(error) = result {
            self.packer.clear();
            self.scratch.fill(0);
            return Err(self.owner.fail(error));
        }
        result
    }

    fn pull_inner(&mut self) -> Result<Batch, Error> {
        let until = self
            .owner
            .check()?
            .checked_add(packing::READ_BUDGET_NS)
            .ok_or(Error::Clock)?;
        for _ in 0..MAX_READ_CHUNKS {
            self.read_time(until)?;
            let packet = self
                .owner
                .native
                .borrow()
                .as_ref()
                .ok_or(Error::Closed)?
                .read(self.scratch.as_mut())?;
            let now = self.read_time(until)?;
            let Some(packet) = packet else {
                break;
            };
            let stamp = self.timeline.stamp(
                packet.position,
                packet.qpc_100ns,
                packet.frames,
                self.channels,
                packet.flags,
                now,
            )?;
            if packet.flags & policy::DISCONTINUITY != 0 {
                self.packer.discontinuity();
            }
            let values = usize::try_from(packet.frames)
                .ok()
                .and_then(|frames| frames.checked_mul(usize::from(self.channels)))
                .filter(|values| *values <= self.scratch.len())
                .ok_or(Error::BufferLimit)?;
            self.packer.push_i16(stamp, &self.scratch[..values], now)?;
            self.scratch[..values].fill(0);
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
                gaps: self.packer.gaps(),
                // WASAPI reports discontinuity without identifying its cause;
                // don't claim that every device gap proves a queue overrun.
                overruns: 0,
            },
        })
    }

    /// Fence all retained frames before stopping the original native stream.
    /// A failed stop is not returned as successful worker cleanup.
    pub fn close(&mut self) -> Result<(), Error> {
        self.packer.clear();
        self.scratch.fill(0);
        self.owner.close()
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
