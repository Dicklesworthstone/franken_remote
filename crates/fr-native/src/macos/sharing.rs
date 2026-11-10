//! Native file controls for the original Mac connection.
//!
//! Progress is metadata, not publication. Only actual terminal receipts can
//! produce a finished label; queued/staged byte counts never do. Polling the
//! buttons stays on the `AppKit` thread; file work stays with the original frd
//! owners. No path, file name or contents enter this UI.
use super::{Window, event_time, files};
use crate::NativeError;
use fr_files::{sender::Outcome, worker::Completion};
use frd::{
    native_files::{Absence, ReceivePhase, SendPhase},
    session_startup::FileSendError,
};
use std::time::Duration;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum State {
    #[default]
    Absent = 0,
    Waiting = 1,
    Active = 2,
    Finished = 3,
    Cancelled = 4,
    Failed = 5,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Progress {
    pub(crate) state: State,
    pub(crate) done: u64,
    pub(crate) total: u64,
}
impl Progress {
    const fn state(state: State) -> Self {
        Self {
            state,
            done: 0,
            total: 0,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct View {
    pub(crate) upload: Progress,
    pub(crate) download: Progress,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Sample {
    pub(crate) actions: u32,
    pub(crate) visible: bool,
}

fn upload(files: &files::Files, native_last: bool) -> Progress {
    use files::Status;
    // Later admitted batches have independent progress and cleanup. Sampling
    // their retained handle never replaces the initial selection's receipts.
    let command = files.command_snapshot();
    let native = files.drop_progress();
    let active = |snapshot: &frd::native_files::SendSnapshot| {
        snapshot.status.absence.is_none()
            && matches!(
                snapshot.status.phase,
                SendPhase::WaitingForControl | SendPhase::Negotiating | SendPhase::Sending
            )
    };
    // Both local selectors use the same one-slot intake. A refused concurrent
    // gesture must not conceal progress of the batch actually being sent.
    let later = if command.as_ref().is_some_and(active) {
        command.map(Ok)
    } else if native.as_ref().is_some_and(|status| match status {
        Ok(snapshot) => active(snapshot),
        Err(code) => *code == "file_selection_preparing",
    }) || native_last
    {
        native.or_else(|| command.map(Ok))
    } else {
        command.map(Ok).or(native)
    };
    let later = match later {
        Some(Ok(snapshot)) => Some(Status::Configured(snapshot)),
        Some(Err("file_selection_preparing")) => return Progress::state(State::Waiting),
        Some(Err("file_selection_stopped")) => return Progress::state(State::Cancelled),
        Some(Err(_)) => return Progress::state(State::Failed),
        None => None,
    };
    let status = match later.or_else(|| files.upload_status()) {
        None => return Progress::default(),
        Some(Status::NotStarted) => return Progress::state(State::Waiting),
        Some(Status::Unavailable(absence)) => {
            return Progress::state(if absence == Absence::Refused(FileSendError::Cancelled) {
                State::Cancelled
            } else {
                State::Failed
            });
        }
        Some(Status::Configured(status)) => status,
    };
    if status.cleanup.is_some_and(|result| result.is_err()) {
        return Progress::state(State::Failed);
    }
    if let Some(report) = status.status.report
        && report.complete
    {
        // The original batch marks complete only after source cleanup. Each
        // successful label additionally needs its recipient publication receipt.
        let published = report.stop.is_none()
            && report.receipts().len() == report.total
            && report
                .receipts()
                .all(|receipt| matches!(receipt.outcome, Outcome::HostPublished { .. }));
        return Progress::state(if published {
            State::Finished
        } else if status.cancel_requested {
            State::Cancelled
        } else {
            State::Failed
        });
    }
    let cancelled_before_source = status.status.phase == SendPhase::Ended
        && status.status.absence == Some(Absence::Refused(FileSendError::Cancelled))
        && status.stage.is_none()
        && status.current.is_none()
        && status.status.report.is_none();
    if status.cancel_requested && (status.cleanup == Some(Ok(())) || cancelled_before_source) {
        return Progress::state(State::Cancelled);
    }
    if status.status.absence.is_some() {
        return Progress::state(State::Failed);
    }
    let state = match status.status.phase {
        SendPhase::WaitingForControl | SendPhase::Negotiating | SendPhase::WaitingForSelection => {
            State::Waiting
        }
        SendPhase::Sending => State::Active,
        SendPhase::SelectionAccepted | SendPhase::Finished | SendPhase::Ended => State::Failed,
    };
    status.current.map_or_else(
        || Progress::state(state),
        |progress| {
            // A directory's size is unknown until the original sender has
            // measured it. Never send an impossible done > total native sample.
            let total = progress.total_bytes.unwrap_or(0);
            Progress {
                state,
                done: progress.queued_bytes.min(total),
                total,
            }
        },
    )
}

fn download(files: &files::Files) -> Progress {
    use files::Status;
    let status = match files.download_status() {
        None => return Progress::default(),
        Some(Status::NotStarted) => return Progress::state(State::Waiting),
        Some(Status::Unavailable(absence)) => {
            return Progress::state(if absence == Absence::Refused(FileSendError::Cancelled) {
                State::Cancelled
            } else {
                State::Failed
            });
        }
        Some(Status::Configured(status)) => status,
    };
    if status.cancel_requested
        && status.phase == ReceivePhase::Ended
        && status.cleanup == Some(Ok(()))
    {
        return Progress::state(State::Cancelled);
    }
    if status.absence.is_some() || status.cleanup.is_some_and(|result| result.is_err()) {
        return Progress::state(State::Failed);
    }
    let state = match status.phase {
        ReceivePhase::WaitingForControl | ReceivePhase::Negotiating => State::Waiting,
        ReceivePhase::Receiving => State::Active,
        ReceivePhase::Ended => {
            let published = status.cleanup == Some(Ok(()))
                && !status.results_truncated
                && !status.results.is_empty()
                && status.results.iter().all(|receipt| {
                    matches!(receipt.outcome, Ok(Completion::Published(publication))
                        if publication.id == receipt.id)
                });
            if published {
                State::Finished
            } else {
                State::Failed
            }
        }
    };
    status.progress.map_or_else(
        || Progress::state(state),
        |progress| Progress {
            state,
            done: progress.staged_bytes,
            total: progress.total_bytes,
        },
    )
}

/// No remote viewport is covered: the window reserves native chrome before
/// its first frame. Original results remain readable after a viewing retry.
pub(crate) fn flags(files: &files::Files) -> u32 {
    u32::from(files.has_upload()) | (u32::from(files.has_download()) << 1)
}

/// Button actions are sampled every event-pump turn. Status snapshots and
/// native label updates are limited to ten per second and skipped if unchanged.
#[derive(Default)]
pub(crate) struct Ui {
    previous: Option<Duration>,
    sampled: Option<Duration>,
    displayed: Option<View>,
    command_batches: usize,
    native_batches: usize,
    native_last: bool,
}
impl Ui {
    pub(crate) fn turn(
        &mut self,
        window: &Window,
        files: &mut files::Files,
    ) -> Result<(), NativeError> {
        if flags(files) == 0 {
            return Ok(());
        }
        let sample = window.sharing_state()?;
        if sample.actions & !3 != 0 || !sample.visible {
            files.cancel();
            return Err(NativeError::Unavailable);
        }
        if sample.actions & 1 != 0 {
            files.cancel_upload();
        }
        if sample.actions & 2 != 0 {
            files.cancel_download();
        }
        let now = event_time().ok_or(NativeError::Unavailable)?;
        if self.previous.is_some_and(|previous| now < previous) {
            files.cancel();
            return Err(NativeError::Unavailable);
        }
        self.previous = Some(now);
        if sample.actions == 0
            && self
                .sampled
                .is_some_and(|sampled| now.saturating_sub(sampled) < Duration::from_millis(100))
        {
            return Ok(());
        }
        self.sampled = Some(now);
        let commands = files
            .command_results()
            .and_then(Result::ok)
            .map_or(0, |results| results.batches().len());
        let native = files.dropped_batches().len();
        if commands != self.command_batches {
            self.native_last = false;
        }
        if native != self.native_batches {
            self.native_last = true;
        }
        self.command_batches = commands;
        self.native_batches = native;
        let view = View {
            upload: upload(files, self.native_last),
            download: download(files),
        };
        if self.displayed != Some(view) {
            window.sharing_update(view)?;
            self.displayed = Some(view);
        }
        Ok(())
    }
}
