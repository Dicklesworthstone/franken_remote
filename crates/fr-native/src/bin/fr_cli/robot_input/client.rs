use super::{Command, Failure, Output};
use crate::session_control::{self, input};
use asupersync::{runtime::RuntimeBuilder, types::Budget};
pub(super) fn run(command: &Command) -> Result<Output, Failure> {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .map_err(|_| Failure::new("runtime_unavailable", "Check local runtime resources.", 1))?;
    let cx = runtime
        .handle()
        .try_request_cx_with_budget(Budget::INFINITE)
        .map_err(|_| Failure::new("runtime_unavailable", "Check local runtime resources.", 1))?;
    let reply = runtime.block_on(input::exchange(&cx,&command.target,&command.connection,command.request)).map_err(|error| {
        let code = match error {
            session_control::Error::NoSession => "no_local_session",
            session_control::Error::WrongOwner => "local_session_changed",
            session_control::Error::UntrustedPeer => "local_session_untrusted",
            session_control::Error::OutcomeUnknown => return Failure::new("robot_input_outcome_unknown", "The original action may have executed. Inspect its retained result using the SAME connection, lease, request ID and original preconditions. Never replay it under a new request.",1).uncertain(),
            _ => "local_robot_input_unavailable",
        };
        Failure::new(code, "Check the exact foreground automation owner and its public connection ID. No remote session was opened.",1)
    })?;
    reply.render(command.json)
}
