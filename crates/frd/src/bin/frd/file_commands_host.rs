//! Lifetime of the original host's local file-command owner.
use super::file_commands::{print_info, report_error};
use frd::native_files::{HostSelection, local};
use std::{
    process::ExitCode,
    thread,
    time::{Duration, Instant},
};

pub(super) fn open(
    target: &str,
    json_output: bool,
) -> Result<(HostSelection, local::Owner), ExitCode> {
    let (selection, intake) = HostSelection::interactive();
    let owner = local::Owner::bind(target, intake.clone()).map_err(|error| {
        intake.close();
        report_error(error, json_output)
    })?;
    let info = owner
        .info()
        .map_err(|error| report_error(error, json_output))?;
    print_info(&info, "endpoint_opened", None, json_output);
    Ok((selection, owner))
}

/// Called after original host teardown; requested stop alone is not a join.
pub(super) fn finish(owner: Option<&local::Owner>) -> bool {
    let Some(owner) = owner else {
        return true;
    };
    owner.stop();
    let Some(until) = Instant::now().checked_add(Duration::from_secs(2)) else {
        return false;
    };
    while !owner.is_complete() && Instant::now() < until {
        thread::sleep(Duration::from_millis(5));
    }
    owner.is_complete()
}
