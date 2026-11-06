//! Client-upload entry points to the same bounded, credential-checked protocol.
use super::{
    Cancellation, Error, Info, Pacing, Scope, Submission, command, identify, set_paused, wire,
};
use std::path::PathBuf;

pub fn inspect(connection: &str) -> Result<Info, Error> {
    let (_, info, _, _) = identify(Scope::Upload, connection)?;
    Ok(info)
}

pub fn status(connection: &str, instance: &str, sequence: Option<u64>) -> Result<Info, Error> {
    if sequence == Some(0) {
        return Err(Error::InvalidRequest);
    }
    command(
        Scope::Upload,
        connection,
        instance,
        wire::Command::Status(sequence),
        &[],
    )
    .map(|reply| reply.0)
}

pub fn send(connection: &str, instance: &str, paths: &[PathBuf]) -> Result<Submission, Error> {
    let payload = wire::paths(paths)?;
    let (info, _) = command(
        Scope::Upload,
        connection,
        instance,
        wire::Command::Send(paths.len()),
        &payload,
    )?;
    Ok(Submission {
        sequence: info.status.sequence,
        info,
    })
}

pub fn cancel(connection: &str, instance: &str, sequence: u64) -> Result<Cancellation, Error> {
    if sequence == 0 {
        return Err(Error::InvalidRequest);
    }
    let (info, requested) = command(
        Scope::Upload,
        connection,
        instance,
        wire::Command::Cancel(sequence),
        &[],
    )?;
    Ok(Cancellation { info, requested })
}

/// Pause one retained request under its original authority and expiry.
pub fn pause(connection: &str, instance: &str, sequence: u64) -> Result<Pacing, Error> {
    set_paused(Scope::Upload, connection, instance, sequence, true)
}
/// Resume the same request; never retry, reopen or select another transfer.
pub fn resume(connection: &str, instance: &str, sequence: u64) -> Result<Pacing, Error> {
    set_paused(Scope::Upload, connection, instance, sequence, false)
}
