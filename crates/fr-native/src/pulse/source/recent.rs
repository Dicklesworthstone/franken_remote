//! Fixed native-worker scratch for one pull, never a second playout pipeline.
use super::{Error, MAX_FRAME_VALUES, PULL_FRAMES};

pub(super) struct Recent {
    frames: Box<[[i16; MAX_FRAME_VALUES]; PULL_FRAMES]>,
    timestamps: [u64; PULL_FRAMES],
    lengths: [usize; PULL_FRAMES],
    first: usize,
    len: usize,
    dropped: u32,
}
impl Recent {
    pub(super) fn new() -> Self {
        Self {
            frames: Box::new([[0; MAX_FRAME_VALUES]; PULL_FRAMES]),
            timestamps: [0; PULL_FRAMES],
            lengths: [0; PULL_FRAMES],
            first: 0,
            len: 0,
            dropped: 0,
        }
    }
    /// Replace oldest RAW work before the encoder sees it. No packet sequence
    /// is invented; the real encoder assigns sequences to retained frames only.
    pub(super) fn push(&mut self, pcm: &[i16], timestamp: u64) -> Result<(), Error> {
        if pcm.is_empty() || pcm.len() > MAX_FRAME_VALUES {
            return Err(Error::BufferLimit);
        }
        let index = (self.first + self.len) % PULL_FRAMES;
        if self.len == PULL_FRAMES {
            self.first = (self.first + 1) % PULL_FRAMES;
            self.dropped = self.dropped.saturating_add(1);
        } else {
            self.len += 1;
        }
        self.frames[index].fill(0);
        self.frames[index][..pcm.len()].copy_from_slice(pcm);
        self.timestamps[index] = timestamp;
        self.lengths[index] = pcm.len();
        Ok(())
    }
    pub(super) const fn dropped(&self) -> u32 {
        self.dropped
    }
    /// Oldest-to-newest order among the surviving frames, original timestamps.
    pub(super) fn iter(&self) -> impl Iterator<Item = (&[i16], u64)> {
        (0..self.len).map(move |offset| {
            let index = (self.first + offset) % PULL_FRAMES;
            (
                &self.frames[index][..self.lengths[index]],
                self.timestamps[index],
            )
        })
    }
    pub(super) fn clear(&mut self) {
        for frame in self.frames.iter_mut() {
            frame.fill(0);
        }
        self.timestamps.fill(0);
        self.lengths.fill(0);
        self.first = 0;
        self.len = 0;
        self.dropped = 0;
    }
}
impl Drop for Recent {
    fn drop(&mut self) {
        self.clear();
    }
}
