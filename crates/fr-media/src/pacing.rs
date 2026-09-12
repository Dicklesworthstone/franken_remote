//! Deterministic raw-capture admission pacing, not a bandwidth estimator.
//!
//! Actual source-work age, transport enqueue pressure and capture credit remain
//! distinct. Unknown/idle samples cannot justify an upward probe. No action here
//! changes codec parameters, discards encoded references or extends a deadline.

const PRESSURE_DWELL_US: u64 = 100_000;
const REDUCE_DWELL_US: u64 = 250_000;
const PROBE_DWELL_US: u64 = 2_000_000;
const IDLE_DWELL_US: u64 = 1_000_000;
const MAX_SAMPLE_GAP_US: u64 = 100_000;
pub const SOURCE_EVIDENCE_US: u64 = 250_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidPolicy,
    InvalidObservation,
    ClockRegression,
    Closed,
}
/// Microseconds on ONE host's monotonic clock. The minimum must also satisfy the
/// configured encoder's frame-rate ceiling, checked by the native stream owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub minimum_interval_us: u64,
    pub maximum_interval_us: u64,
}
impl Policy {
    fn validate(self) -> Result<(), Error> {
        if self.minimum_interval_us < 1_000
            || self.minimum_interval_us > self.maximum_interval_us
            || self.maximum_interval_us > 1_000_000
            || (self.maximum_interval_us > 200_000
                && self.minimum_interval_us != self.maximum_interval_us)
        {
            return Err(Error::InvalidPolicy);
        }
        Ok(())
    }
    fn conservative(self) -> u64 {
        (self.minimum_interval_us * 2).min(self.maximum_interval_us)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Unknown,
    Ready,
    Blocked,
}
/// An actual conditional capture's original observation time, NEVER receipt or
/// heartbeat time. `changed=false` means verified unchanged, not missing output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub at_us: u64,
    pub changed: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub now_us: u64,
    /// Queued + executing + uncollected source work, not encoder-only latency.
    /// A recently completed operation may be retained for `SOURCE_EVIDENCE_US`.
    pub source_work_us: Option<u64>,
    /// Actual enqueue outcome. Accepted is NOT delivered or measured capacity.
    pub send: Availability,
    /// Credit for another maximal encoded result, including reference retention.
    pub capture_credit: Availability,
    pub observation: Option<Observation>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Active,
    Idle,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Hold,
    SourceWork,
    SendAdmission,
    CaptureCredit,
    ReceiverBacklog,
    DecoderWork,
    HeadroomProbe,
    VerifiedIdle,
    ChangedAfterIdle,
    EvidenceGap,
}
/// Optional remote evidence. Unnegotiated peers retain local-only behavior;
/// negotiated peers without a timely solicited sample cannot justify a probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverEvidence {
    Unobserved,
    Unknown,
    Measured(fr_wire::receiver_metrics::Load),
}
/// Bounded content-free decision evidence. Elapsed durations expose the dwell
/// evidence; replaying the same ordered samples produces exactly the same reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub sample: Sample,
    pub previous_interval_us: u64,
    pub interval_us: u64,
    pub mode: Mode,
    pub reason: Reason,
    pub pressure_us: [u64; 3],
    pub headroom_us: u64,
    pub receiver: ReceiverEvidence,
    pub receiver_pressure_us: [u64; 2],
}
/// Fixed-size state and bounded adjustment history. Callers may export reports through
/// their own bounded diagnostic sink; this controller never retains media.
#[derive(Debug)]
pub struct Controller {
    policy: Policy,
    interval: u64,
    last_now: Option<u64>,
    adjusted: Option<u64>,
    busy_since: [Option<u64>; 3],
    receiver_busy_since: [Option<u64>; 2],
    headroom_since: Option<u64>,
    headroom_evidence_us: u64,
    headroom_previous_ready: bool,
    source: Option<Observation>,
    unchanged_since: Option<u64>,
    mode: Mode,
    closed: bool,
    report: Option<Report>,
    decisions: [Option<Report>; 16],
    decision_next: usize,
}
impl Controller {
    pub fn new(policy: Policy) -> Result<Self, Error> {
        policy.validate()?;
        Ok(Self {
            policy,
            interval: policy.conservative(),
            last_now: None,
            adjusted: None,
            busy_since: [None; 3],
            receiver_busy_since: [None; 2],
            headroom_since: None,
            headroom_evidence_us: 0,
            headroom_previous_ready: false,
            source: None,
            unchanged_since: None,
            mode: Mode::Active,
            closed: false,
            report: None,
            decisions: [None; 16],
            decision_next: 0,
        })
    }
    pub const fn policy(&self) -> Policy {
        self.policy
    }
    pub const fn interval_us(&self) -> u64 {
        self.interval
    }
    pub const fn report(&self) -> Option<Report> {
        self.report
    }
    /// Last sixteen adjustments in chronological order, without heap allocation.
    pub fn decisions(&self) -> impl Iterator<Item = &Report> {
        (0..self.decisions.len()).filter_map(move |i| {
            self.decisions[(self.decision_next + i) % self.decisions.len()].as_ref()
        })
    }
    pub fn update(&mut self, sample: Sample) -> Result<Report, Error> {
        self.update_with_receiver(sample, ReceiverEvidence::Unobserved)
    }
    pub fn update_with_receiver(
        &mut self,
        sample: Sample,
        receiver: ReceiverEvidence,
    ) -> Result<Report, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        match self.update_inner(sample, receiver) {
            Ok(report) => {
                self.report = Some(report);
                if report.reason != Reason::Hold {
                    self.decisions[self.decision_next] = Some(report);
                    self.decision_next = (self.decision_next + 1) % self.decisions.len();
                }
                Ok(report)
            }
            Err(error) => {
                self.closed = true;
                Err(error)
            }
        }
    }
    fn update_inner(&mut self, s: Sample, receiver: ReceiverEvidence) -> Result<Report, Error> {
        let previous = self.interval;
        if self.last_now.is_some_and(|t| s.now_us < t) {
            return Err(Error::ClockRegression);
        }
        let gap = self
            .last_now
            .is_some_and(|t| s.now_us - t > MAX_SAMPLE_GAP_US);
        self.last_now = Some(s.now_us);
        let mut reason = Reason::Hold;
        if gap {
            self.reset_evidence();
            self.mode = Mode::Active;
            self.interval = self.interval.max(self.policy.conservative());
            self.adjusted = Some(s.now_us);
            reason = Reason::EvidenceGap;
        }
        if self
            .source
            .is_some_and(|o| s.now_us - o.at_us > SOURCE_EVIDENCE_US)
        {
            self.unchanged_since = None;
            self.reset_headroom();
            if self.mode == Mode::Idle {
                self.mode = Mode::Active;
                reason = Reason::EvidenceGap;
            }
        }
        let changed = self.observe(s.observation, s.now_us)?;
        let (pressure, pressure_us) = self.track_pressure(s);
        let (remote, receiver_ready, receiver_pressure_us) =
            self.track_receiver(receiver, s.now_us);
        let fresh = self
            .source
            .is_some_and(|o| s.now_us - o.at_us <= SOURCE_EVIDENCE_US);
        let idle = fresh
            && self.source.is_some_and(|o| {
                !o.changed
                    && self
                        .unchanged_since
                        .is_some_and(|t| o.at_us - t >= IDLE_DWELL_US)
            });
        if changed && self.mode == Mode::Idle {
            self.mode = Mode::Active;
            // Wake is conservative, never an optimistic probe through pressure.
            if !pressure.contains(&true) && !remote.contains(&true) {
                self.interval = self.policy.conservative();
            }
            self.adjusted = Some(s.now_us);
            self.reset_headroom();
            reason = Reason::ChangedAfterIdle;
        } else if idle && self.mode != Mode::Idle {
            self.mode = Mode::Idle;
            self.interval = self.policy.maximum_interval_us;
            self.adjusted = Some(s.now_us);
            self.reset_headroom();
            reason = Reason::VerifiedIdle;
        }
        let headroom = receiver_ready
            && fresh
            && !idle
            && self.mode == Mode::Active
            && s.source_work_us
                .is_some_and(|work| work <= self.interval * 3 / 4)
            && s.send == Availability::Ready
            && s.capture_credit == Availability::Ready;
        // A short outstanding source operation can make credit unknown. It
        // contributes no headroom duration, but must not erase every preceding
        // measured interval and make recovery impossible in a real pipeline.
        let uncertain = !remote.contains(&true)
            && fresh
            && !idle
            && self.mode == Mode::Active
            && s.source_work_us
                .is_none_or(|work| work <= self.interval * 3 / 4)
            && s.send != Availability::Blocked
            && s.capture_credit != Availability::Blocked;
        let headroom_us = self.track_headroom(s.now_us, headroom, uncertain);
        if reason == Reason::Hold && self.mode == Mode::Active {
            reason = self.adjust_for_load(s.now_us, pressure_us, receiver_pressure_us, headroom_us);
        }
        Ok(Report {
            sample: s,
            previous_interval_us: previous,
            interval_us: self.interval,
            mode: self.mode,
            reason,
            pressure_us,
            headroom_us,
            receiver,
            receiver_pressure_us,
        })
    }
    fn track_receiver(
        &mut self,
        receiver: ReceiverEvidence,
        now_us: u64,
    ) -> ([bool; 2], bool, [u64; 2]) {
        let (remote, receiver_ready) = match receiver {
            ReceiverEvidence::Unobserved => ([false; 2], true),
            ReceiverEvidence::Unknown => ([false; 2], false),
            ReceiverEvidence::Measured(load) => (
                [
                    load.retained_pictures > 1,
                    load.work_us.is_some_and(|n| n >= self.interval),
                ],
                !load.decoding
                    && load.retained_pictures <= 1
                    && load.work_us.is_some_and(|n| n <= self.interval * 3 / 4),
            ),
        };
        let mut receiver_pressure_us = [0; 2];
        for (i, busy) in remote.into_iter().enumerate() {
            if busy {
                receiver_pressure_us[i] =
                    now_us - *self.receiver_busy_since[i].get_or_insert(now_us);
            } else {
                self.receiver_busy_since[i] = None;
            }
        }
        (remote, receiver_ready, receiver_pressure_us)
    }
    fn adjust_for_load(
        &mut self,
        now_us: u64,
        pressure_us: [u64; 3],
        receiver_pressure_us: [u64; 2],
        headroom_us: u64,
    ) -> Reason {
        let since_adjust = self.adjusted.map_or(u64::MAX, |t| now_us - t);
        if let Some(index) = pressure_us
            .iter()
            .chain(receiver_pressure_us.iter())
            .position(|&t| t >= PRESSURE_DWELL_US)
            && since_adjust >= REDUCE_DWELL_US
            && self.interval < self.policy.maximum_interval_us
        {
            self.interval = (self.interval * 2).min(self.policy.maximum_interval_us);
            self.adjusted = Some(now_us);
            self.reset_headroom();
            return [
                Reason::SourceWork,
                Reason::SendAdmission,
                Reason::CaptureCredit,
                Reason::ReceiverBacklog,
                Reason::DecoderWork,
            ][index];
        } else if headroom_us >= PROBE_DWELL_US
            && since_adjust >= PROBE_DWELL_US
            && self.interval > self.policy.minimum_interval_us
        {
            self.interval =
                (self.interval - self.interval.div_ceil(8)).max(self.policy.minimum_interval_us);
            self.adjusted = Some(now_us);
            self.reset_headroom();
            self.headroom_since = Some(now_us);
            self.headroom_previous_ready = true;
            return Reason::HeadroomProbe;
        }
        Reason::Hold
    }
    fn reset_headroom(&mut self) {
        self.headroom_since = None;
        self.headroom_evidence_us = 0;
        self.headroom_previous_ready = false;
    }
    fn track_headroom(&mut self, now: u64, ready: bool, uncertain: bool) -> u64 {
        if self
            .headroom_since
            .is_some_and(|last| now - last > MAX_SAMPLE_GAP_US)
        {
            self.reset_headroom();
        }
        if ready {
            if self.headroom_previous_ready
                && let Some(last) = self.headroom_since
            {
                self.headroom_evidence_us = self.headroom_evidence_us.saturating_add(now - last);
            }
            self.headroom_since = Some(now);
            self.headroom_previous_ready = true;
        } else if uncertain {
            // Neither edge adjacent to an unknown interval is counted.
            self.headroom_previous_ready = false;
        } else {
            self.reset_headroom();
        }
        if ready { self.headroom_evidence_us } else { 0 }
    }
    fn track_pressure(&mut self, s: Sample) -> ([bool; 3], [u64; 3]) {
        let pressure = [
            s.source_work_us.is_some_and(|work| work >= self.interval),
            s.send == Availability::Blocked,
            s.capture_credit == Availability::Blocked,
        ];
        let mut pressure_us = [0; 3];
        for (index, busy) in pressure.into_iter().enumerate() {
            if busy {
                let start = self.busy_since[index].get_or_insert(s.now_us);
                pressure_us[index] = s.now_us - *start;
            } else {
                self.busy_since[index] = None;
            }
        }
        (pressure, pressure_us)
    }
    fn observe(&mut self, incoming: Option<Observation>, now: u64) -> Result<bool, Error> {
        let Some(o) = incoming else { return Ok(false) };
        if o.at_us > now
            || self.source.is_some_and(|old| {
                o.at_us < old.at_us || (o.at_us == old.at_us && o.changed != old.changed)
            })
        {
            return Err(Error::InvalidObservation);
        }
        if self.source == Some(o) {
            return Ok(false);
        }
        self.source = Some(o);
        if o.changed || now - o.at_us > SOURCE_EVIDENCE_US {
            self.unchanged_since = None;
        } else {
            self.unchanged_since.get_or_insert(o.at_us);
        }
        Ok(o.changed)
    }
    fn reset_evidence(&mut self) {
        self.busy_since = [None; 3];
        self.receiver_busy_since = [None; 2];
        self.reset_headroom();
        self.unchanged_since = None;
    }
}
