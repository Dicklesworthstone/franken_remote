//! One local upload choice through the existing Linux portal request owner.
//! The caller pins these paths before opening any remote session.
use super::{Cell, Cx, Failure, Runtime, Shutdown};
use fr_native::directory_picker::{Error, FilePicker};
use std::{path::PathBuf, time::Duration};

pub(super) fn select(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
) -> Result<Vec<PathBuf>, Failure> {
    if stopped.get() || cx.is_cancel_requested() {
        return Err(refused(Error::Cancelled));
    }
    let mut picker = FilePicker::open().map_err(refused)?;
    let selected = runtime.block_on(shutdown.run(cx, stopped, wait(cx, &mut picker)));
    // The original portal request must have ended even on local cancellation.
    // No unresolved chooser can fall through to an upload or a connection.
    picker.close().map_err(refused)?;
    if stopped.get() || cx.is_cancel_requested() {
        return Err(refused(Error::Cancelled));
    }
    selected.map_err(refused)
}

async fn wait(cx: &Cx, picker: &mut FilePicker) -> Result<Vec<PathBuf>, Error> {
    let timer = cx.timer_driver().ok_or(Error::Unavailable)?;
    loop {
        if cx.is_cancel_requested() {
            return Err(Error::Cancelled);
        }
        if let Some(paths) = picker.poll()? {
            return Ok(paths);
        }
        asupersync::time::sleep(timer.now(), Duration::from_millis(10)).await;
    }
}

fn refused(error: Error) -> Failure {
    let (code, next, exit) = match error {
        Error::Cancelled => (
            "file_choice_cancelled",
            "Local file selection was cancelled. No remote connection or control request was opened.",
            130,
        ),
        Error::Expired => (
            "file_choice_expired",
            "The original local file choice expired. Select files again; no remote connection was opened.",
            1,
        ),
        Error::InvalidSelection => (
            "file_choice_invalid",
            "Choose one to eight distinct local files within the selection limits. No remote connection was opened.",
            2,
        ),
        Error::CleanupIncomplete => (
            "file_picker_cleanup_incomplete",
            "The original file picker did not confirm cleanup. No remote connection was opened.",
            1,
        ),
        Error::EventFlood => (
            "file_choice_event_overflow",
            "The file picker exceeded its bounded event allowance. No selection was accepted or remote connection opened.",
            1,
        ),
        _ => (
            "file_picker_unavailable",
            "The Linux desktop file chooser is unavailable. Use a graphical session with an XDG FileChooser portal, or explicit --send PATH. No remote connection was opened.",
            1,
        ),
    };
    Failure::new(code, next, exit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_choice_refusals_preserve_cancellation_and_cleanup_meanings() {
        for (error, code, exit) in [
            (Error::Cancelled, "file_choice_cancelled", 130),
            (Error::Expired, "file_choice_expired", 1),
            (Error::InvalidSelection, "file_choice_invalid", 2),
            (
                Error::CleanupIncomplete,
                "file_picker_cleanup_incomplete",
                1,
            ),
            (Error::EventFlood, "file_choice_event_overflow", 1),
            (Error::Unavailable, "file_picker_unavailable", 1),
        ] {
            let failure = refused(error);
            assert_eq!(failure.code, code);
            assert_eq!(failure.exit, exit);
            assert!(!failure.next.contains("receive_directory"));
        }
    }
}
