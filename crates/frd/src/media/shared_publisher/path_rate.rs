//! The shared source's encoder rate follows its viewers' paths (fr-7al9).
//!
//! The capture gate (`join.rs`) already skips captures while no viewer with
//! credit has a clear path. Below about half the encoder's rate that is not
//! enough: a queue that every viewer's path keeps holding lowers the rate
//! toward what those paths deliver (`fr_media::bitrate::Controller::observe_path_evidence`).
//! One encoder serves every viewer, so a slow viewer never lowers a healthy
//! viewer's rate: while some paths are queued and others are clear, the rate
//! holds. With `--adaptive-capture` the same evidence reaches that pacing's
//! own rate controller instead.
use super::{Entry, Error, MAX_SUBSCRIBERS, MediaError, Members, ObservationControl, Publisher};
use fr_core::authority::SessionAuthority;
use fr_media::bitrate::{self, PathEvidence};
use std::sync::{Arc, Mutex, Weak};

/// Original observation owners, each immutably bound to one connection at
/// subscriber admission. Weak identities cannot retain or renew their consent.
/// A recovery changes decoder bindings, not this transport cohort.
#[derive(Default)]
pub(super) struct PathCohort {
    owners: [Option<Weak<Mutex<SessionAuthority>>>; MAX_SUBSCRIBERS],
}
impl PathCohort {
    pub(super) fn observe(
        &mut self,
        controller: &mut bitrate::Controller,
        members: &Members,
        now: u64,
    ) -> Result<(), Error> {
        let owners = std::array::from_fn(|slot| {
            members.entries[slot]
                .as_ref()
                .filter(|entry| entry.path_eligible())
                .map(|entry| Arc::downgrade(&entry.control.authority))
        });
        if self
            .owners
            .iter()
            .zip(&owners)
            .any(|(old, current)| match (old, current) {
                (Some(old), Some(current)) => !Weak::ptr_eq(old, current),
                (None, None) => false,
                _ => true,
            })
        {
            controller.reset_evidence().map_err(bitrate_error)?;
            self.owners = owners;
        }
        observe(controller, now, members.paths(now)).map_err(bitrate_error)
    }
}

impl Entry {
    fn path_eligible(&self) -> bool {
        self.failure.is_none()
            && self.starting.is_none()
            && (self.join.is_none() || self.recovery.is_some())
    }
}

/// The rate controller of a source without adaptive pacing.
pub(super) struct PathRate {
    controller: bitrate::Controller,
    capture: Option<bitrate::Capture>,
    cohort: PathCohort,
}

/// The paths of the viewers a source serves, in one service turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Paths {
    /// Every eligible viewer has recent acknowledged busy delivery, no queue.
    Clear,
    /// No queue, but idle, recovering or missing delivery evidence cannot probe.
    Unmeasured,
    /// Every eligible viewer's path holds one. The lowest delivery any of them
    /// measured while busy, or 0 when none did.
    Queued(u64),
    /// Some do and some do not, or no viewer is eligible.
    Split,
}

/// Only a whole measured cohort can raise the shared encoder's rate.
fn classify(paths: impl Iterator<Item = PathEvidence>) -> Paths {
    let (mut queued, mut clear, mut unmeasured) = (0_usize, 0_usize, 0_usize);
    let mut delivered: Option<u64> = None;
    for evidence in paths {
        match evidence {
            PathEvidence::Queued {
                delivered_bits_per_second: rate,
            } => {
                queued += 1;
                if rate > 0 {
                    delivered = Some(delivered.map_or(rate, |lowest| lowest.min(rate)));
                }
            }
            PathEvidence::Clear => clear += 1,
            PathEvidence::Unknown | PathEvidence::Idle => unmeasured += 1,
        }
    }
    match (queued, clear, unmeasured) {
        (0, 0, 0) => Paths::Split,
        (_, 0, 0) => Paths::Queued(delivered.unwrap_or(0)),
        (0, _, 0) => Paths::Clear,
        (0, _, _) => Paths::Unmeasured,
        _ => Paths::Split,
    }
}

impl Members {
    fn paths(&self, now: u64) -> Paths {
        let paths = classify(
            self.entries
                .iter()
                .flatten()
                .filter(|entry| entry.path_eligible())
                .map(|entry| entry.path.evidence(now)),
        );
        if paths == Paths::Clear
            && self
                .entries
                .iter()
                .flatten()
                .any(|entry| entry.path_eligible() && !entry.pacing_eligible())
        {
            // A recovered connection can drain before the replacement picture
            // is usable. Its ACKs are not permission to raise the source rate.
            Paths::Unmeasured
        } else {
            paths
        }
    }
}

fn observe(
    controller: &mut bitrate::Controller,
    now: u64,
    paths: Paths,
) -> Result<(), bitrate::Error> {
    match paths {
        Paths::Clear => controller.observe_path_evidence(now, PathEvidence::Clear),
        Paths::Unmeasured => controller.observe_path_evidence(now, PathEvidence::Unknown),
        Paths::Queued(delivered_bits_per_second) => controller.observe_path_evidence(
            now,
            PathEvidence::Queued {
                delivered_bits_per_second,
            },
        ),
        Paths::Split => controller.observe_split_path(now),
    }
}

impl PathRate {
    fn new(initial: u32) -> Result<Self, Error> {
        Ok(Self {
            controller: bitrate::Controller::new(initial).map_err(bitrate_error)?,
            capture: None,
            cohort: PathCohort::default(),
        })
    }
    pub(super) const fn controller(&self) -> &bitrate::Controller {
        &self.controller
    }
    /// A change of original owners voids the evidence; reference recovery on
    /// those same connections retains measured congestion and native intent.
    pub(super) fn observe_members(&mut self, members: &Members) -> Result<(), Error> {
        self.cohort
            .observe(&mut self.controller, members, members.last)
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
        // A subscriber can sample its path after this publisher turn began.
        // Judge that snapshot at the owner clock already observed under the
        // same lock, so a fresh sample cannot appear to come from the future.
        let now = now.max(members.last);
        if let Some(rate) = &mut self.path_rate {
            rate.cohort.observe(&mut rate.controller, &members, now)?;
        } else if let Some(adaptive) = &mut self.adaptive {
            adaptive.observe_paths(&members, now)?;
        }
        Ok(())
    }
}

fn bitrate_error(error: bitrate::Error) -> Error {
    Error::Media(MediaError::Bitrate(error))
}

#[cfg(test)]
impl Publisher {
    pub(crate) fn capture_interval_for_test(&mut self, now: u64) -> Result<Option<u64>, Error> {
        self.capture_interval(now)
    }

    /// Explicit estimator fixture on real admitted owners. Each pair is a
    /// baseline and a fresh ACK snapshot; only this measurement is synthetic.
    /// Session clocks, membership, capture credit and IPC remain production.
    pub(crate) fn sample_paths_for_test(
        &mut self,
        loads: &[(fr_transport::quic::PathLoad, fr_transport::quic::PathLoad)],
    ) -> Result<(), Error> {
        let now = {
            let mut members = self.members.lock().map_err(|_| Error::Poisoned)?;
            members.tick()?;
            let now = members.last;
            assert_eq!(members.entries.iter().flatten().count(), loads.len());
            for (entry, (baseline, fresh)) in members.entries.iter_mut().flatten().zip(loads) {
                entry.path.observe(None, now.saturating_sub(1));
                entry.path.observe(Some(*baseline), now.saturating_sub(1));
                entry.path.observe(Some(*fresh), now);
            }
            now
        };
        self.observe_paths(now)
    }
}

#[cfg(test)]
mod tests {
    use super::{Paths, classify, observe};
    use fr_media::bitrate::{Controller, PathEvidence, Reason};

    fn queued(delivered_bits_per_second: u64) -> PathEvidence {
        PathEvidence::Queued {
            delivered_bits_per_second,
        }
    }

    #[test]
    fn only_a_whole_queued_cohort_is_queued() {
        assert_eq!(classify([].into_iter()), Paths::Split, "nobody to judge");
        assert_eq!(
            classify([PathEvidence::Clear, PathEvidence::Clear].into_iter()),
            Paths::Clear
        );
        assert_eq!(
            classify([queued(900_000), queued(0), queued(700_000)].into_iter()),
            Paths::Queued(700_000),
            "the slowest measured path"
        );
        assert_eq!(classify([queued(0)].into_iter()), Paths::Queued(0));
        assert_eq!(
            classify([queued(700_000), PathEvidence::Clear].into_iter()),
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
        for now in (0..=200_000).step_by(20_000) {
            observe(&mut c, now, Paths::Queued(0)).unwrap();
        }
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

    #[test]
    fn idle_and_unknown_members_never_supply_shared_probe_headroom() {
        for evidence in [PathEvidence::Idle, PathEvidence::Unknown] {
            assert_eq!(
                classify([PathEvidence::Clear, evidence].into_iter()),
                Paths::Unmeasured
            );
            assert_eq!(classify([evidence].into_iter()), Paths::Unmeasured);
            assert_eq!(
                classify([queued(800_000), evidence].into_iter()),
                Paths::Split
            );
            let mut c = Controller::new(2_000_000).unwrap();
            for now in (0..=200_000).step_by(20_000) {
                observe(&mut c, now, Paths::Queued(800_000)).unwrap();
            }
            let capture = c.begin_capture().unwrap();
            c.complete_capture(capture, capture.bitrate(), true, 200_000)
                .unwrap();
            for now in (200_000..10_000_000).step_by(20_000) {
                let paths = classify([PathEvidence::Clear, evidence].into_iter());
                observe(&mut c, now, paths).unwrap();
                assert_eq!(c.pending(), None);
            }
        }
    }
}
