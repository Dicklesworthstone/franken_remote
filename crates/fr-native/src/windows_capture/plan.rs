//! Decide useful Windows capture work before touching the native rate property.
//! A proposal alone cannot create source evidence or wake an idle desktop.
use super::freshness::{Capture, Observation};
use crate::capture_rate::Rate;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Plan {
    Refuse,
    Unchanged,
    Encode { bitrate: u32, force_idr: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Picture {
    First,
    Following,
    Recovery,
}

pub(super) struct Work {
    pub observation: Observation,
    pub pointer_changed: bool,
    pub only_if_changed: bool,
    pub picture: Picture,
}
impl Work {
    pub fn plan(&self, rate: &Rate, host: u64, worker: Instant) -> Plan {
        let force_idr = self.picture != Picture::Following;
        match self.observation.capture(force_idr || !self.only_if_changed) {
            Capture::Refuse => return Plan::Refuse,
            Capture::Unchanged if !self.pointer_changed => return Plan::Unchanged,
            Capture::Unchanged | Capture::Encode => {}
        }
        let bitrate = rate
            .due(host, worker, self.picture != Picture::First)
            .unwrap_or(rate.applied_bitrate());
        Plan::Encode {
            bitrate,
            force_idr: force_idr || bitrate != rate.applied_bitrate(),
        }
    }
}
