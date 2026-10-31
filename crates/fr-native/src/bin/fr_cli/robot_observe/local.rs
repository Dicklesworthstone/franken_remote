//! Query one existing local publisher without opening any remote connection.
use super::{Failure, Minimum, bounded_args, observing, usage};
use fr_client::robot::live::{Evidence, Snapshot};
use std::{collections::BTreeSet, path::PathBuf};

struct Request {
    target: String,
    instance: String,
    maximum_age_ms: Option<u64>,
    minimum: Minimum,
    json: bool,
    screenshot: Option<PathBuf>,
}
fn parse(args: &[String]) -> Result<Request, Failure> {
    bounded_args(args)?;
    if !observing(args) {
        return Err(usage());
    }
    let target = args
        .get(2)
        .filter(|s| !s.is_empty() && s.len() <= 253 && !s.starts_with('-'))
        .ok_or_else(usage)?
        .clone();
    let mut request = Request {
        target,
        instance: String::new(),
        maximum_age_ms: None,
        minimum: Minimum::Decoded,
        json: false,
        screenshot: None,
    };
    let mut index = 3;
    let mut seen = BTreeSet::new();
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        if !seen.insert(flag) {
            return Err(usage());
        }
        if flag == "--json" {
            request.json = true;
            continue;
        }
        let value = args
            .get(index)
            .filter(|value| !value.is_empty() && !value.starts_with("--"))
            .ok_or_else(usage)?;
        index += 1;
        match flag {
            "--screenshot" => request.screenshot = Some(PathBuf::from(value)),
            "--connection-id" => {
                if value.len() != 32
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(usage());
                }
                request.instance.clone_from(value);
            }
            "--max-receipt-age-ms" => {
                let age: u64 = value.parse().map_err(|_| usage())?;
                if !(1..=60_000).contains(&age) {
                    return Err(usage());
                }
                request.maximum_age_ms = Some(age);
            }
            "--evidence-level" => {
                request.minimum = match value.as_str() {
                    "decoded" => Minimum::Decoded,
                    "submitted_to_compositor" => Minimum::SubmittedToCompositor,
                    "instrumentally_observed" => {
                        return Err(Failure::new(
                            "observation_evidence_unavailable",
                            "This local owner exposes native receipts, not optical visibility or application results.",
                            2,
                        ));
                    }
                    _ => return Err(usage()),
                }
            }
            _ => return Err(usage()),
        }
    }
    if request.instance.is_empty() {
        return Err(usage());
    }
    Ok(request)
}

pub(super) fn run(args: &[String]) -> Result<String, Failure> {
    let request = parse(args)?;
    #[cfg(target_os = "linux")]
    {
        use crate::session_control::observation;
        use asupersync::{runtime::RuntimeBuilder, types::Budget};
        let runtime = RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .map_err(|_| {
                Failure::new("runtime_unavailable", "Check local runtime resources.", 1)
            })?;
        let cx = runtime
            .handle()
            .try_request_cx_with_budget(Budget::INFINITE)
            .map_err(|_| {
                Failure::new("runtime_unavailable", "Check local runtime resources.", 1)
            })?;
        if request.screenshot.is_some() {
            let observed = runtime
                .block_on(crate::session_control::screenshot::read(
                    &cx,
                    &request.target,
                    &request.instance,
                ))
                .map_err(artifact::read_failure)?;
            let request = Request {
                target: observed.info.target,
                instance: observed.info.instance,
                ..request
            };
            return artifact::render(
                &request,
                observed.metadata,
                &observed.snapshot,
                observed.round_trip_ms,
            );
        }
        let observed = runtime
            .block_on(observation::read(&cx, &request.target, &request.instance))
            .map_err(read_failure)?;
        let request = Request {
            target: observed.info.target,
            instance: observed.info.instance,
            ..request
        };
        render(&request, &observed.snapshot, observed.round_trip_ms)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = request;
        Err(Failure::new(
            "platform_unavailable",
            "Authenticated local observation is available on Linux.",
            2,
        ))
    }
}

fn render(request: &Request, snapshot: &Snapshot, round_trip_ms: u64) -> Result<String, Failure> {
    snapshot.validate().map_err(|_| {
        Failure::new(
            "local_session_protocol",
            "The owner returned invalid observation metadata.",
            1,
        )
    })?;
    let frame = snapshot.frame.as_ref().ok_or_else(|| Failure::new(
        "observation_not_ready", "This owner has no active decoded observation. Check its foreground process, host approval and display selection; no new connection was opened.", 1))?;
    let age = frame
        .receipt_age_ms
        .checked_add(round_trip_ms)
        .ok_or_else(|| {
            Failure::new(
                "observation_age_unavailable",
                "The local receipt age could not be bounded.",
                1,
            )
        })?;
    if request.maximum_age_ms.is_some_and(|maximum| age > maximum) {
        return Err(Failure::new(
            "observation_receipt_too_old",
            "The retained receipt exceeds the requested local age bound. Observe again after a real new frame; no recapture, reconnect or retry was performed.",
            1,
        ));
    }
    if request.minimum == Minimum::SubmittedToCompositor
        && frame.evidence_level != Evidence::SubmittedToCompositor
    {
        return Err(Failure::new(
            "observation_evidence_insufficient",
            "The actual receipt proves decoding only, not compositor submission. No stronger evidence is inferred.",
            1,
        ));
    }
    if request.json {
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": 1, "timestamp_unix_ms": crate::output::timestamp(),
            "outcome": "success",
            "data": {
                "scope": "local_session_observation", "target": request.target,
                "connection_id": request.instance, "snapshot": snapshot,
                "receipt_age_upper_ms": age, "query_round_trip_ms": round_trip_ms,
                "source_freshness": "unknown", "capture_timestamp_unix_ms": null,
                "configuration_generation": null, "control_authority": null,
                "current_input_precondition": false, "physical_visibility_proven": false,
                "new_connection_opened": false
            }
        }))
        .map_err(|_| {
            Failure::new(
                "serialization_error",
                "Could not render the local observation.",
                1,
            )
        })
    } else {
        Ok(format!(
            "Local session observation: {}\nConnection ID: {}\nAttempt: {}\nDisplay handle: {}\nGeometry generation: {} ({}x{})\nFrame: {} ({:?})\nLocal receipt age upper bound: {} ms\nHost source freshness: unknown. No input authority or physical visibility is implied.\n",
            request.target,
            request.instance,
            snapshot.attempt,
            frame.display_handle,
            frame.geometry_generation,
            frame.pixel_width,
            frame.pixel_height,
            frame.frame_serial,
            frame.evidence_level,
            age,
        ))
    }
}

#[cfg(target_os = "linux")]
fn read_failure(error: crate::session_control::observation::ReadError) -> Failure {
    use crate::session_control::{Error, observation::ReadError};
    let code = match error {
        ReadError::Local(Error::InvalidInstance) => "invalid_connection_id",
        ReadError::Local(Error::InvalidTarget) => "invalid_local_target",
        ReadError::Local(Error::NoSession) => "no_local_session",
        ReadError::Local(Error::WrongOwner) => "local_session_changed",
        ReadError::Local(Error::UntrustedPeer) => "local_session_untrusted",
        ReadError::Local(Error::Protocol) => "local_session_protocol",
        ReadError::Local(Error::Timeout) => "local_session_timeout",
        ReadError::Local(Error::Cancelled) => {
            return Failure::new(
                "cancelled",
                "Local observation lookup was cancelled; no stop or input was sent.",
                130,
            );
        }
        ReadError::Unavailable | ReadError::Local(_) => "local_observation_unavailable",
    };
    Failure::new(
        code,
        "Use the exact target and connection_id from fr robot session status. The original foreground view must opt in with --publish-observations; this query never creates a replacement session.",
        1,
    )
}

#[cfg(test)]
#[path = "local_tests.rs"]
mod tests;

#[cfg(target_os = "linux")]
#[path = "local/artifact.rs"]
mod artifact;
