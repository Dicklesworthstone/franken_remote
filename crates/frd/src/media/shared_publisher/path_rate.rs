//! The shared source's encoder rate follows its viewers' paths (fr-7al9).
//!
//! The capture gate (`join.rs`) already skips captures while no viewer with
//! credit has a clear path. Below about half the encoder's rate that is not
//! enough: a queue that every viewer's path keeps holding lowers the rate
//! toward what those paths deliver (`fr_media::bitrate::Controller::observe_path`).
//! One encoder serves every viewer, so a slow viewer never lowers a healthy
//! viewer's rate: while some paths are queued and others are clear, the rate
//! holds. With `--adaptive-capture` the same evidence reaches that pacing's
//! own rate controller instead.
use super::{Error, MAX_SUBSCRIBERS, MediaError, Members, ObservationControl, Publisher};
use fr_media::bitrate;
use fr_wire::decoder::Binding;

/// The rate controller of a source without adaptive pacing.
pub(super) struct PathRate {
    controller: bitrate::Controller,
    capture: Option<bitrate::Capture>,
    views: [Option<Binding>; MAX_SUBSCRIBERS],
}

/// The paths of the viewers a source serves, in one service turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Paths {
    /// No eligible viewer's path holds a queue.
    Clear,
    /// Every eligible viewer's path holds one. The lowest delivery any of them
    /// measured while busy, or 0 when none did.
    Queued(u64),
    /// Some do and some do not, or no viewer is eligible.
    Split,
}

/// Each eligible viewer's path: whether it holds a queue, and its busy delivery.
fn classify(paths: impl Iterator<Item = (bool, u64)>) -> Paths {
    let (mut queued, mut clear) = (0_usize, 0_usize);
    let mut delivered: Option<u64> = None;
    for (is_queued, rate) in paths {
        if is_queued {
            queued += 1;
            if rate > 0 {
                delivered = Some(delivered.map_or(rate, |lowest| lowest.min(rate)));
            }
        } else {
            clear += 1;
        }
    }
    match (queued, clear) {
        (0, 0) => Paths::Split,
        (_, 0) => Paths::Queued(delivered.unwrap_or(0)),
        (0, _) => Paths::Clear,
        _ => Paths::Split,
    }
}

impl Members {
    fn paths(&self) -> Paths {
        classify(
            self.entries
                .iter()
                .flatten()
                .filter(|entry| entry.pacing_eligible())
                .map(|entry| (entry.path.queued(), entry.path.delivered_bits_per_second())),
        )
    }
    fn rate_views(&self) -> [Option<Binding>; MAX_SUBSCRIBERS] {
        let mut views = [None; MAX_SUBSCRIBERS];
        for (view, entry) in views.iter_mut().zip(&self.entries) {
            *view = entry
                .as_ref()
                .filter(|entry| entry.pacing_eligible())
                .map(|entry| entry.view);
        }
        views
    }
}

fn observe(
    controller: &mut bitrate::Controller,
    now: u64,
    paths: Paths,
) -> Result<(), bitrate::Error> {
    match paths {
        Paths::Clear => controller.observe_path(now, false, 0),
        Paths::Queued(delivered) => controller.observe_path(now, true, delivered),
        Paths::Split => controller.observe_split_path(now),
    }
}

impl PathRate {
    fn new(initial: u32) -> Result<Self, Error> {
        Ok(Self {
            controller: bitrate::Controller::new(initial).map_err(bitrate_error)?,
            capture: None,
            views: [None; MAX_SUBSCRIBERS],
        })
    }
    pub(super) const fn controller(&self) -> &bitrate::Controller {
        &self.controller
    }
    /// A change of viewers voids the evidence: an unsent proposal is cancelled,
    /// and a target native work already kept is restored on the next capture.
    pub(super) fn observe_views(&mut self, members: &Members) -> Result<(), Error> {
        let views = members.rate_views();
        if views != self.views {
            self.controller.reset_evidence().map_err(bitrate_error)?;
            self.views = views;
        }
        Ok(())
    }
    /// The rate this capture requests. Only actual pool and member credit may
    /// begin one.
    pub(super) fn begin(&mut self) -> Result<u32, Error> {
        if self.capture.is_some() {
            return Err(Error::InvalidBudget);
        }
        let capture = self.controller.begin_capture().map_err(bitrate_error)?;
        self.capture = Some(capture);
        Ok(capture.bitrate())
    }
    /// Record the worker's actual rate before expiry or fanout can reject this
    /// picture. A deferred target is not an applied change.
    pub(super) fn complete(
        &mut self,
        owner: &ObservationControl,
        update: &crate::media::SharedCaptureUpdate,
    ) -> Result<(), Error> {
        let capture = self.capture.take().ok_or(Error::InvalidBudget)?;
        let completed = crate::media::host_now(&owner.context())
            .map_err(Error::Media)?
            .as_micros();
        if update.is_skipped() {
            if update.applied_bitrate().is_some() {
                return Err(Error::Media(MediaError::InvalidFrame));
            }
            return self
                .controller
                .skip_capture(
                    capture,
                    update
                        .skipped_bitrate()
                        .ok_or(Error::Media(MediaError::InvalidFrame))?,
                    completed,
                )
                .map_err(bitrate_error);
        }
        let actual = update
            .applied_bitrate()
            .ok_or(Error::Media(MediaError::InvalidFrame))?;
        self.controller
            .complete_capture(
                capture,
                actual,
                update.encoded().is_some_and(|frame| frame.kind().is_idr()),
                completed,
            )
            .map_err(bitrate_error)
    }
}

impl Publisher {
    /// Default on for a worker implementing the complete capture-rate
    /// transaction; enable after any adaptive capture and before service.
    /// With adaptive pacing its own rate controller takes the path evidence.
    pub(crate) fn enable_path_bitrate(&mut self) -> Result<bool, Error> {
        self.members.lock().map_err(|_| Error::Poisoned)?.tick()?;
        if self.served || self.path_rate.is_some() {
            return Err(Error::InvalidBudget);
        }
        if let Some(adaptive) = &mut self.adaptive {
            return Ok(adaptive.bitrate_mut().is_some());
        }
        if !self.source.configuration.backend.accepts_rate_capture() {
            return Ok(false);
        }
        self.path_rate = Some(Box::new(PathRate::new(self.source.configuration.bitrate)?));
        Ok(true)
    }
    /// Once per service turn, after each viewer's own path sample.
    pub(super) fn observe_paths(&mut self, now: u64) -> Result<(), Error> {
        let members = self.members.lock().map_err(|_| Error::Poisoned)?;
        if let Some(rate) = &mut self.path_rate {
            rate.observe_views(&members)?;
            observe(&mut rate.controller, now, members.paths()).map_err(bitrate_error)?;
        } else if let Some(controller) = self.adaptive.as_mut().and_then(|a| a.bitrate_mut()) {
            observe(controller, now, members.paths()).map_err(bitrate_error)?;
        }
        Ok(())
    }
}

fn bitrate_error(error: bitrate::Error) -> Error {
    Error::Media(MediaError::Bitrate(error))
}

#[cfg(test)]
mod tests {
    use super::{Paths, classify, observe};
    use fr_media::bitrate::{Controller, Reason};

    #[test]
    fn only_a_whole_queued_cohort_is_queued() {
        assert_eq!(classify([].into_iter()), Paths::Split, "nobody to judge");
        assert_eq!(classify([(false, 0), (false, 9)].into_iter()), Paths::Clear);
        assert_eq!(
            classify([(true, 900_000), (true, 0), (true, 700_000)].into_iter()),
            Paths::Queued(700_000),
            "the slowest measured path"
        );
        assert_eq!(classify([(true, 0)].into_iter()), Paths::Queued(0));
        assert_eq!(
            classify([(true, 700_000), (false, 0)].into_iter()),
            Paths::Split,
            "a healthy viewer keeps the rate"
        );
    }

    #[test]
    fn a_split_cohort_neither_cuts_nor_probes() {
        let mut c = Controller::new(2_000_000).unwrap();
        // 150 ms queued, then a healthy viewer appears beside the slow one.
        for now in (0..=150_000).step_by(10_000) {
            observe(&mut c, now, Paths::Queued(800_000)).unwrap();
        }
        for now in (160_000..10_000_000).step_by(10_000) {
            observe(&mut c, now, Paths::Split).unwrap();
            assert_eq!(c.pending(), None);
        }
        // The queue interval starts over: 200 ms of a whole queued cohort.
        for now in (10_000_000..10_200_000).step_by(10_000) {
            observe(&mut c, now, Paths::Queued(800_000)).unwrap();
            assert_eq!(c.pending(), None);
        }
        observe(&mut c, 10_200_000, Paths::Queued(800_000)).unwrap();
        let cut = c.pending().unwrap();
        assert_eq!(cut.reason(), Reason::PathQueue);
        assert_eq!(cut.bitrate(), 480_000);
    }

    #[test]
    fn a_split_cohort_cancels_an_unsent_probe() {
        let mut c = Controller::new(2_000_000).unwrap();
        observe(&mut c, 0, Paths::Queued(0)).unwrap();
        observe(&mut c, 100_000, Paths::Queued(0)).unwrap();
        observe(&mut c, 200_000, Paths::Queued(0)).unwrap();
        let capture = c.begin_capture().unwrap();
        c.complete_capture(capture, capture.bitrate(), true, 200_000)
            .unwrap();
        for now in (200_000..=4_200_000).step_by(20_000) {
            observe(&mut c, now, Paths::Clear).unwrap();
        }
        assert_eq!(c.pending().unwrap().reason(), Reason::Headroom);
        observe(&mut c, 4_220_000, Paths::Split).unwrap();
        assert_eq!(c.pending(), None);
    }
}
