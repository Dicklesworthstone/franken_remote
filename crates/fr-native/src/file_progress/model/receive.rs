//! Original receive-owner evidence. Staging and cleanup never imply publication.
use super::{Action, Frame, Publication, line};
use fr_files::{session, wire::ResultReceipt, worker};
use frd::native_files::{Absence, MAX_RECEIVE_RESULTS, ReceivePhase, ReceiveStatus};

impl Frame {
    pub(super) fn receive(status: &ReceiveStatus) -> Self {
        let mut frame = Self {
            lines: [[0; super::COLUMNS]; super::ROWS],
            progress: -1,
            actions: if !status.cancel_requested && status.phase != ReceivePhase::Ended {
                Action::Cancel as u32
            } else {
                0
            },
        };
        frame.lines[0] = line(format_args!("{}", phase(status.phase)));
        frame.lines[1] = line(format_args!(
            "Request: {} | Only the original download can be cancelled",
            if status.cancel_requested {
                "cancel"
            } else {
                "receive"
            }
        ));
        frame.lines[2] = match status.progress {
            Some(progress) if progress.id != 0 => {
                if progress.total_bytes != 0 {
                    frame.progress = i32::try_from(
                        u128::from(progress.staged_bytes.min(progress.total_bytes)) * 1000
                            / u128::from(progress.total_bytes),
                    )
                    .expect("ratio is bounded to 1000");
                }
                line(format_args!(
                    "Selection #{} staged: {} / {} bytes",
                    progress.id - 1,
                    progress.staged_bytes,
                    progress.total_bytes
                ))
            }
            Some(_) => line(format_args!(
                "Current progress has an invalid selection index"
            )),
            None => line(format_args!(
                "Current selection size / staged bytes not yet available"
            )),
        };
        let truncated = status.results_truncated || status.results.len() > MAX_RECEIVE_RESULTS;
        let mut outcomes = Outcomes::default();
        for receipt in status.results.iter().take(MAX_RECEIVE_RESULTS) {
            outcomes.observe(receipt);
        }
        frame.lines[3] = line(format_args!(
            "Results: {} retained | {}",
            status.results.len().min(MAX_RECEIVE_RESULTS),
            if truncated {
                "Additional results omitted; missing results may have effects"
            } else {
                "No additional results omitted"
            }
        ));
        frame.lines[4] = line(format_args!(
            "Published locally: {} | Durability unknown: {}",
            outcomes.published, outcomes.durability_unknown
        ));
        frame.lines[5] = line(format_args!(
            "Refused: {} | Cancelled: {} | Publication unknown: {}",
            outcomes.refused, outcomes.cancelled, outcomes.unknown
        ));
        frame.lines[6] = line(format_args!("Disk cleanup: {}", cleanup(status)));
        frame.lines[7] = line(format_args!("{}", end(status.absence)));
        frame
    }
}

fn phase(phase: ReceivePhase) -> &'static str {
    match phase {
        ReceivePhase::WaitingForControl => "Waiting for control; no download started",
        ReceivePhase::Negotiating => "Negotiating the original download lane",
        ReceivePhase::Receiving => "Receiving / waiting for the host's next explicit selection",
        ReceivePhase::Ended => "Download lane ended; retained outcomes are reported below",
    }
}

fn cleanup(status: &ReceiveStatus) -> &'static str {
    match status.cleanup {
        Some(Ok(())) => "complete; earlier publications remain",
        Some(Err(_)) => "failed; earlier publications remain",
        None if status.may_have_started() => "pending / not yet reported",
        None => "not started",
    }
}

fn end(absence: Option<Absence>) -> &'static str {
    match absence {
        Some(Absence::NotNegotiated) => "The host did not enable the download capability",
        Some(Absence::WithClipboard) => "The peer cannot combine this file lane with clipboard",
        Some(Absence::Unavailable) => {
            "The original download attempt is unavailable; no automatic retry"
        }
        Some(Absence::Refused(_)) => {
            "The download was refused locally; inspect the terminal result"
        }
        Some(Absence::SetupFailed(_)) => "Download attachment failed; inspect the terminal result",
        Some(Absence::ReceiveFailed(fr_files::quic::Error::Cancelled)) => {
            "Download cancelled; earlier publications and cleanup are shown separately"
        }
        Some(Absence::ReceiveFailed(_)) => {
            "Download lane failed; inspect retained and terminal results"
        }
        None => "Staged bytes do not prove publication. Original deadlines keep running.",
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
struct Outcomes {
    published: usize,
    durability_unknown: usize,
    refused: usize,
    cancelled: usize,
    unknown: usize,
}
impl Outcomes {
    fn observe(&mut self, receipt: &ResultReceipt) {
        match receipt.outcome {
            // Progress is never a terminal result, even at 100% or zero bytes.
            Ok(worker::Completion::Begun(_) | worker::Completion::Written(_)) => {}
            Ok(worker::Completion::Published(published))
                if receipt.id != 0 && published.id == receipt.id =>
            {
                self.published += 1;
                self.durability_unknown +=
                    usize::from(published.publication == Publication::DurabilityUnknown);
            }
            Ok(worker::Completion::Cancelled)
            | Err(
                worker::Error::Cancelled
                | worker::Error::Closed
                | worker::Error::Session(session::Error::Closed),
            ) => self.cancelled += 1,
            Ok(worker::Completion::Published(_))
            | Err(
                worker::Error::UnknownEffect
                | worker::Error::Poisoned
                | worker::Error::Panicked
                | worker::Error::Session(session::Error::UnknownEffect),
            ) => self.unknown += 1,
            Err(_) => self.refused += 1,
        }
    }
}

#[cfg(test)]
mod tests;
