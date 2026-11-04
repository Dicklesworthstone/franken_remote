//! Local layout changes and retained original interruption/closing evidence.
use super::{CallbackError, Progress};

/// Called only after the original native gesture and Session's cleanup gate.
/// A display choice is not a second use of command-line input/file consent.
pub(super) fn record(progress: &mut Progress, next: u8) -> Result<(), CallbackError> {
    if !(2..=32).contains(&next)
        || progress.attempts.checked_add(1) != Some(next)
        || progress.display_change_closes.len() + progress.window_resizes.len() >= 31
        || progress
            .window_resizes
            .last()
            .is_some_and(|(attempt, _, _)| *attempt >= progress.attempts)
        || progress
            .display_change_closes
            .last()
            .is_some_and(|(attempt, _)| *attempt >= progress.attempts)
        || progress.user_closed
        || progress.automation.is_some()
        || progress.automation_focus_stop.is_some()
    {
        return Err(CallbackError);
    }
    let summary = progress.cleanup.ok_or(CallbackError)?;
    if summary.failure.is_some() {
        return Err(CallbackError);
    }
    if let Some(control) = &mut progress.control {
        control.retire_initial_decision();
    }
    progress
        .display_change_closes
        .push((progress.attempts, summary));
    progress.display_changes += 1;
    Ok(())
}

/// A verified resize is a new fitted observation, not a new use of the
/// command-line input, clipboard or file decision. Failure history is retained.
pub(super) fn record_resize(
    progress: &mut Progress,
    next: u8,
    size: (u32, u32),
) -> Result<(), CallbackError> {
    if !(2..=32).contains(&next)
        || progress.attempts.checked_add(1) != Some(next)
        || progress.display_change_closes.len() + progress.window_resizes.len() >= 31
        || progress
            .display_change_closes
            .last()
            .is_some_and(|(attempt, _)| *attempt >= progress.attempts)
        || progress
            .window_resizes
            .last()
            .is_some_and(|(attempt, _, _)| *attempt >= progress.attempts)
        || progress.user_closed
        || progress.automation.is_some()
        || progress.automation_focus_stop.is_some()
        || fr_media::worker::presentation::X11Target::new(1, size.0, size.1).is_err()
    {
        return Err(CallbackError);
    }
    let summary = progress.cleanup.ok_or(CallbackError)?;
    if summary.failure.is_some()
        || summary.input_stop.is_some_and(|reason| {
            !matches!(
                reason,
                fr_native::viewer_input::StopReason::Local
                    | fr_native::viewer_input::StopReason::Closed
                    | fr_native::viewer_input::StopReason::WindowResized
            )
        })
        || summary.retry_failure().is_some()
    {
        return Err(CallbackError);
    }
    if let Some(control) = &mut progress.control {
        control.retire_initial_decision();
    }
    progress
        .window_resizes
        .push((progress.attempts, size, summary));
    Ok(())
}

/// Content-free reports remain visible even if selection or a later dial fails.
pub(super) fn history(progress: &Progress) -> crate::ControlHistory {
    use std::fmt::Write;
    let mut reports = String::new();
    let mut text = String::new();
    for (attempt, summary) in &progress.display_change_closes {
        let exchange = summary
            .disconnect
            .or_else(|| summary.control_disconnect.map(|outcome| outcome.exchange));
        let lease = summary
            .control_disconnect
            .and_then(|outcome| outcome.revocation);
        if !reports.is_empty() {
            reports.push(',');
        }
        let _ = write!(
            reports,
            "{{\"attempt\":{attempt},\"close_exchange\":{},\"lease_report\":{}}}",
            exchange.map_or_else(
                || "null".to_owned(),
                |outcome| crate::closure::Report::from(outcome).json()
            ),
            crate::terminal::lease_json(lease)
        );
        let _ = write!(
            text,
            " Display change after attempt {attempt}: {}",
            exchange.map_or_else(
                || "No completed close exchange; host cleanup remains unknown.".to_owned(),
                |outcome| crate::closure::Report::from(outcome).text()
            )
        );
        if let Some(lease) = lease {
            let _ = write!(
                text,
                " Lease report: {}.",
                crate::terminal::lease_text(lease)
            );
        }
    }
    let mut json = format!(
        ",\"display_changes\":{},\"display_change_closes\":[{reports}]",
        progress.display_changes
    );
    if !progress.window_resizes.is_empty() {
        let mut resizes = String::new();
        for (attempt, (width, height), summary) in &progress.window_resizes {
            let exchange = summary
                .disconnect
                .or_else(|| summary.control_disconnect.map(|outcome| outcome.exchange));
            let lease = summary
                .control_disconnect
                .and_then(|outcome| outcome.revocation);
            if !resizes.is_empty() {
                resizes.push(',');
            }
            let _ = write!(
                resizes,
                "{{\"attempt\":{attempt},\"width\":{width},\"height\":{height},\"close_exchange\":{},\"lease_report\":{}}}",
                exchange.map_or_else(
                    || "null".to_owned(),
                    |outcome| crate::closure::Report::from(outcome).json()
                ),
                crate::terminal::lease_json(lease)
            );
            let _ = write!(
                text,
                " Window resize after attempt {attempt}: new viewing viewport {width}x{height}; original input and auxiliary choices ended. {}",
                exchange.map_or_else(
                    || "No completed close exchange; host cleanup remains unknown.".to_owned(),
                    |outcome| crate::closure::Report::from(outcome).text()
                )
            );
        }
        let _ = write!(json, ",\"window_resizes\":[{resizes}]");
    }
    crate::ControlHistory { json, text }
}

#[cfg(test)]
#[path = "display_change/tests.rs"]
mod tests;
