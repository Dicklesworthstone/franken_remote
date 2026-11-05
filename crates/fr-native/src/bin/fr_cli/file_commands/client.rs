//! Authenticate the original foreground owner before addressing its file lane.
use super::{
    Output,
    options::{Action, Options},
    report,
};
use crate::{Failure, session_control};
use asupersync::{runtime::RuntimeBuilder, types::Budget};
use frd::native_files::local::upload;

pub(super) fn run(command: &Options) -> Result<Output, Failure> {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .map_err(|_| Failure::new("runtime_unavailable", "Check local runtime resources.", 1))?;
    let cx = runtime
        .handle()
        .try_request_cx_with_budget(Budget::INFINITE)
        .map_err(|_| Failure::new("runtime_unavailable", "Check local runtime resources.", 1))?;
    // The existing foreground handshake authenticates BOTH kernel UID and the
    // full selected node ID. A file socket alone never selects another target.
    let original = runtime.block_on(session_control::probe(&cx, &command.target))
        .map_err(|error| {
            let code = match error {
                session_control::Error::NoSession => "no_local_session",
                session_control::Error::WrongOwner => "local_session_changed",
                session_control::Error::UntrustedPeer => "local_session_untrusted",
                _ => "local_session_unavailable",
            };
            Failure::new(code, "Check the exact foreground owner with fr robot session status NODE_ID. No new connection was opened or file command sent.", 1)
        })?;
    if original.instance != command.connection {
        return Err(Failure::new(
            "local_session_changed",
            "The original foreground connection has ended. No command was sent to its replacement.",
            1,
        ));
    }
    let result = match (&command.action, command.lease.as_deref()) {
        (Action::Status(None), None) => {
            upload::inspect(&command.connection).map(|info| (info, "status", None))
        }
        (Action::Status(sequence), Some(lease)) => {
            upload::status(&command.connection, lease, *sequence).map(|info| (info, "status", None))
        }
        (Action::Send(paths), Some(lease)) => upload::send(&command.connection, lease, paths)
            .map(|sent| (sent.info, "send_admitted", None)),
        (Action::Cancel(sequence), Some(lease)) => {
            upload::cancel(&command.connection, lease, *sequence).map(|cancelled| {
                (
                    cancelled.info,
                    "cancel_requested",
                    Some(cancelled.requested),
                )
            })
        }
        _ => unreachable!("parser requires the original file instance"),
    };
    Ok(match result {
        Ok((info, operation, cancelled)) => report::success(command, &info, operation, cancelled),
        Err(error) => report::failure(command, error),
    })
}
