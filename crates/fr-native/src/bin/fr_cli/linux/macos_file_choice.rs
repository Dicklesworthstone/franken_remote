//! Initial native file choice. No host lookup, connection or control request
//! exists while this panel is open; no active viewer focus exemption is needed.
use super::{Cell, Cx, Failure, Runtime, Shutdown};
use fr_native::macos::file_picker::{Error, FilePicker};
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
    fr_native::macos::initialize_application().map_err(|_| refused(Error::NativeFailure))?;
    let mut picker = FilePicker::open().map_err(refused)?;
    let selected = runtime.block_on(shutdown.run(cx, stopped, wait(cx, &mut picker)));
    // Even cancelled/error outcomes positively close the original native owner
    // before returning. An incomplete cleanup can never fall through to connect.
    picker.close().map_err(refused)?;
    if stopped.get() || cx.is_cancel_requested() {
        return Err(refused(Error::Cancelled));
    }
    selected
}

async fn wait(cx: &Cx, picker: &mut FilePicker) -> Result<Vec<PathBuf>, Failure> {
    let timer = cx
        .timer_driver()
        .ok_or_else(|| refused(Error::NativeFailure))?;
    loop {
        if cx.is_cancel_requested() {
            return Err(refused(Error::Cancelled));
        }
        if let Some(paths) = picker.poll().map_err(refused)? {
            return Ok(paths);
        }
        asupersync::time::sleep(timer.now(), Duration::from_millis(10)).await;
    }
}

fn refused(error: Error) -> Failure {
    let (code, next, exit) = match error {
        Error::Cancelled => (
            "file_choice_cancelled",
            "File selection was cancelled. No remote connection or control request was opened.",
            130,
        ),
        Error::Expired => (
            "file_choice_expired",
            "The original file-choice deadline expired. Make a new local selection; no remote connection was opened.",
            1,
        ),
        Error::InvalidSelection => (
            "file_choice_invalid",
            "Choose 1 to 8 distinct local files or folders within the bounded selection size. No remote connection was opened.",
            2,
        ),
        Error::CleanupIncomplete => (
            "file_picker_cleanup_incomplete",
            "The original file picker did not confirm cleanup. No remote connection was opened.",
            1,
        ),
        Error::EventFlood => (
            "file_choice_event_overflow",
            "The native file picker could not finish its bounded event turn. No selection was accepted or remote connection opened.",
            1,
        ),
        _ => (
            "file_picker_unavailable",
            "The native file picker could not complete. Run from a logged-in graphical macOS session; no remote connection was opened.",
            1,
        ),
    };
    Failure::new(code, next, exit)
}
