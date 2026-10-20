//! Bounded, content-free presentation of ONE original transfer snapshot.
mod receive;
use super::{Action, Snapshot};
use fr_files::{
    receive::Publication,
    sender::{Outcome, Stage},
};
use frd::native_files::{SendPhase, SendSnapshot};
use std::fmt::{self, Write};

pub(super) const ROWS: usize = 8;
pub(super) const COLUMNS: usize = 104;
/// Identical to `fr_upload_frame` at the narrow C drawing boundary. No pointers,
/// paths, peer text, transfer IDs, callbacks or ownership cross this boundary.
#[repr(C)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Frame {
    pub(super) lines: [[u8; COLUMNS]; ROWS],
    /// -1 means unknown, including an empty object awaiting a receipt. Otherwise
    /// this is current-object queued (upload) or staged (download) progress in
    /// thousandths, never completion.
    pub(super) progress: i32,
    pub(super) actions: u32,
}
impl Frame {
    pub(super) fn snapshot(snapshot: &Snapshot) -> Self {
        match snapshot {
            Snapshot::Upload(status) => Self::new(status),
            Snapshot::Download(status) => Self::receive(status),
        }
    }
    pub(super) fn new(s: &SendSnapshot) -> Self {
        let active = !s.cancel_requested
            && !matches!(s.status.phase, SendPhase::Finished | SendPhase::Ended)
            && s.status
                .report
                .is_none_or(|r| !r.complete && r.stop.is_none());
        let mut f = Self {
            lines: [[0; COLUMNS]; ROWS],
            progress: -1,
            actions: 0,
        };
        if active {
            f.actions = Action::Cancel as u32
                | if s.pause_requested {
                    Action::Resume as u32
                } else {
                    Action::Pause as u32
                };
        }
        f.lines[0] = line(format_args!("{}", phase(s)));
        f.lines[1] = line(format_args!(
            "Request: {} | Last observed pause gate: {}",
            if s.cancel_requested {
                "cancel"
            } else if s.pause_requested {
                "pause"
            } else {
                "run"
            },
            match s.paused {
                None => "unknown",
                Some(true) => "paused",
                Some(false) => "running",
            }
        ));
        f.lines[2] = match s.current {
            Some(p) => match p.total_bytes {
                Some(total) => {
                    if total != 0 {
                        f.progress = i32::try_from(
                            u128::from(p.queued_bytes.min(total)) * 1000 / u128::from(total),
                        )
                        .expect("ratio is bounded to 1000");
                    }
                    line(format_args!(
                        "Current object queued: {} / {total} bytes",
                        p.queued_bytes
                    ))
                }
                None => line(format_args!(
                    "Current object queued: {} bytes / size unknown",
                    p.queued_bytes
                )),
            },
            None => line(format_args!(
                "Current object size / queued bytes not yet available"
            )),
        };
        let mut outcomes = Outcomes::default();
        f.lines[3] = if let Some(report) = s.status.report {
            for receipt in report.receipts() {
                outcomes.observe(receipt.outcome);
            }
            line(format_args!(
                "Selections started: {} / {} | Receipts: {} | Batch stop: {}",
                report.started,
                report.total,
                report.receipts().len(),
                if report.stop.is_some() { "yes" } else { "no" }
            ))
        } else {
            line(format_args!(
                "No batch report yet; no publication evidence available"
            ))
        };
        f.lines[4] = line(format_args!(
            "Publication receipts: host {}, controller {}, durability unknown {}",
            outcomes.host, outcomes.controller, outcomes.durability_unknown
        ));
        f.lines[5] = line(format_args!(
            "Publication unknown: {} | Refused: {} | Interrupted: {}",
            outcomes.unknown, outcomes.refused, outcomes.interrupted
        ));
        f.lines[6] = line(format_args!(
            "Local source cleanup: {}",
            match s.cleanup {
                None => "not yet reported",
                Some(Ok(())) => "complete (not proof of remote effects)",
                Some(Err(_)) => "error; consult the terminal receipt",
            }
        ));
        f.lines[7] = line(format_args!(
            "Queued bytes are not publications. Original deadlines keep running."
        ));
        f
    }
    pub(super) fn permits(&self, action: Action) -> bool {
        self.actions & action as u32 != 0
    }
}

fn phase(s: &SendSnapshot) -> &'static str {
    match s.status.phase {
        SendPhase::WaitingForControl => "Waiting for control; no source started",
        SendPhase::Negotiating => "Negotiating the original file lane",
        SendPhase::Finished => "Selection finished; outcomes are reported below",
        SendPhase::Ended => "File lane ended or unavailable; no retry",
        SendPhase::Sending => match s.stage {
            Some(Stage::Queued) => "Next selection queued",
            Some(Stage::Preparing) => "Preparing / hashing the current selection",
            Some(Stage::AwaitingAcceptance) => "Waiting for host acceptance",
            Some(Stage::Streaming) => "Sending the current selection",
            Some(Stage::AwaitingProof) => "Waiting for host publication receipt",
            Some(Stage::Finished | Stage::Closed) => "Retiring the current source",
            Some(Stage::Idle) | None => "Waiting for sender progress",
        },
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
struct Outcomes {
    host: usize,
    controller: usize,
    durability_unknown: usize,
    refused: usize,
    interrupted: usize,
    unknown: usize,
}
impl Outcomes {
    fn observe(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::HostPublished { publication, .. } => {
                self.host += 1;
                self.durability_unknown +=
                    usize::from(publication == Publication::DurabilityUnknown);
            }
            Outcome::ControllerPublished { publication, .. } => {
                self.controller += 1;
                self.durability_unknown +=
                    usize::from(publication == Publication::DurabilityUnknown);
            }
            Outcome::HostRefused(_) | Outcome::ControllerRefused(_) => self.refused += 1,
            Outcome::InterruptedBeforePublication(_) => self.interrupted += 1,
            Outcome::PublicationUnknown => self.unknown += 1,
        }
    }
}

struct Text {
    bytes: [u8; COLUMNS],
    used: usize,
}
impl Write for Text {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // Reserve the NUL sentinel. This is also checked by the C boundary.
        if !s.bytes().all(|b| (32..=126).contains(&b)) || s.len() >= COLUMNS - self.used {
            return Err(fmt::Error);
        }
        self.bytes[self.used..self.used + s.len()].copy_from_slice(s.as_bytes());
        self.used += s.len();
        Ok(())
    }
}
fn line(args: fmt::Arguments<'_>) -> [u8; COLUMNS] {
    let mut text = Text {
        bytes: [0; COLUMNS],
        used: 0,
    };
    if text.write_fmt(args).is_err() {
        // Fail closed without truncating a number into misleading evidence.
        text.bytes = [0; COLUMNS];
        let fallback = b"Status exceeds the panel's bound; consult the terminal receipt";
        text.bytes[..fallback.len()].copy_from_slice(fallback);
    }
    text.bytes
}

#[cfg(test)]
mod tests;
