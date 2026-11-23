//! One local portal folder choice, completed before the native session opens.
//! The selected pathname is never authority: the caller immediately validates
//! and pins it with the original receive directory owner.
use super::{Cell, Cx, Failure, Runtime, Shutdown};
use fr_native::directory_picker::{DirectoryPicker, Error};
use std::{path::PathBuf, time::Duration};

pub(super) fn select(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
) -> Result<PathBuf, Failure> {
    if stopped.get() || cx.is_cancel_requested() {
        return Err(refused(Error::Cancelled));
    }
    let mut picker = DirectoryPicker::open().map_err(refused)?;
    let selected = runtime.block_on(shutdown.run(cx, stopped, wait(cx, &mut picker)));
    // Cancellation and errors retain the same request until its native close
    // is confirmed. An unresolved local chooser cannot fall through to connect.
    picker.close().map_err(refused)?;
    if stopped.get() || cx.is_cancel_requested() {
        return Err(refused(Error::Cancelled));
    }
    selected.map_err(refused)
}

async fn wait(cx: &Cx, picker: &mut DirectoryPicker) -> Result<PathBuf, Error> {
    let timer = cx.timer_driver().ok_or(Error::Unavailable)?;
    loop {
        if cx.is_cancel_requested() {
            return Err(Error::Cancelled);
        }
        if let Some(path) = picker.poll()? {
            return Ok(path);
        }
        asupersync::time::sleep(timer.now(), Duration::from_millis(10)).await;
    }
}

fn refused(error: Error) -> Failure {
    let (code, next, exit) = match error {
        Error::Cancelled => (
            "receive_directory_choice_cancelled",
            "Receiving-folder selection was cancelled. No remote connection or control request was opened.",
            130,
        ),
        Error::Expired => (
            "receive_directory_choice_expired",
            "The original receiving-folder choice expired. Choose a folder again; no remote connection was opened.",
            1,
        ),
        Error::InvalidSelection => (
            "receive_directory_choice_invalid",
            "Choose exactly one existing private local folder. No remote connection was opened.",
            2,
        ),
        Error::CleanupIncomplete => (
            "receive_directory_picker_cleanup_incomplete",
            "The original receiving-folder picker did not confirm cleanup. No remote connection was opened.",
            1,
        ),
        Error::EventFlood => (
            "receive_directory_choice_event_overflow",
            "The receiving-folder picker exceeded its bounded event allowance. No selection was accepted or remote connection opened.",
            1,
        ),
        _ => (
            "receive_directory_picker_unavailable",
            "The Linux desktop file chooser is unavailable. Use a graphical session with an XDG FileChooser portal, or select an explicit private directory with --receive DIR. No remote connection was opened.",
            1,
        ),
    };
    Failure::new(code, next, exit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_refusals_keep_original_choice_and_cleanup_meanings() {
        for (error, code, exit) in [
            (Error::Cancelled, "receive_directory_choice_cancelled", 130),
            (Error::Expired, "receive_directory_choice_expired", 1),
            (
                Error::InvalidSelection,
                "receive_directory_choice_invalid",
                2,
            ),
            (
                Error::CleanupIncomplete,
                "receive_directory_picker_cleanup_incomplete",
                1,
            ),
            (
                Error::EventFlood,
                "receive_directory_choice_event_overflow",
                1,
            ),
            (
                Error::Unavailable,
                "receive_directory_picker_unavailable",
                1,
            ),
        ] {
            let failure = refused(error);
            assert_eq!(failure.code, code);
            assert_eq!(failure.exit, exit);
            assert!(
                failure.next.contains("No remote connection")
                    || failure.next.contains("no remote connection")
                    || failure.next.contains("No selection")
            );
        }
    }
}
