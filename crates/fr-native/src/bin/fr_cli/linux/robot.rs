//! `fr status`, `fr inspect`, `fr disconnect` and `fr robot`.
//!
//! Status and inspect report only what the installed Tailscale `LocalAPI`
//! returns; anything not probed is unknown, never a plausible default. Robot
//! session-status identifies the authenticated original local foreground owner,
//! not a remote grant. Local disconnect and robot session-close wait for that
//! owner's actual native cleanup result. A comparison-only connection ID can
//! prevent an old automation request from stopping a replacement invocation.
//! Session-open, observation and input automation still need live integration.

use super::super::options::{DisconnectOptions, InspectOptions, RobotCommand};
use super::{Cell, Cx, Failure, LocalApi, Runtime, Shutdown, failure, session_control, tailnet};
use fr_client::robot::{AcknowledgementStage, RobotEnvelope, RobotInspectData, RobotStatusData};

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

pub(super) fn run_status(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    api: &LocalApi,
    json: bool,
) -> Result<String, Failure> {
    let node = runtime.block_on(shutdown.run(cx, stopped, api.node_identity(cx)));
    if stopped.get() {
        return Err(Failure::new("cancelled", "Status check cancelled.", 130));
    }
    let node = node.map_err(tailnet)?;
    let data = RobotStatusData {
        client_version: env!("CARGO_PKG_VERSION").into(),
        tailnet_connected: true,
        local_node_name: node.certificate_name().to_string(),
        local_ip: node
            .addresses()
            .first()
            .map_or_else(String::new, ToString::to_string),
        // Sessions live in foreground `fr connect` processes; none is tracked here.
        active_sessions_count: None,
        active_host: None,
        active_role: None,
        active_lease_handle: None,
    };
    let envelope = RobotEnvelope::success(now_unix_ms(), AcknowledgementStage::Observed, data);
    if json {
        envelope
            .render_json()
            .map_err(|_| failure("serialization_error", "Failed to render status JSON."))
    } else {
        Ok(envelope.render_human(
            envelope
                .data
                .as_ref()
                .map(RobotStatusData::render_human)
                .as_deref(),
        ))
    }
}

pub(super) fn run_inspect(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    api: &LocalApi,
    opts: &InspectOptions,
    json: bool,
) -> Result<String, Failure> {
    let snapshot = runtime
        .block_on(shutdown.run(cx, stopped, api.discover(cx)))
        .map_err(tailnet)?;
    if stopped.get() {
        return Err(Failure::new("cancelled", "Host inspection cancelled.", 130));
    }
    let peer = snapshot.peers().iter().find(|p| exact_match(p, opts));
    let Some(p) = peer else {
        return Err(failure(
            "peer_not_found",
            "No machine with exactly that stable ID (or, with --by-name, canonical name) is in installed Tailscale discovery; run 'fr hosts'.",
        ));
    };
    let (transport_path, is_derp) = match p.transport_path() {
        fr_tailnet::PeerTransportPath::Direct { cur_addr } => {
            (format!("direct ({cur_addr})"), false)
        }
        fr_tailnet::PeerTransportPath::DerpRelayed { relay } => {
            (format!("derp_relayed ({relay})"), true)
        }
        fr_tailnet::PeerTransportPath::Unknown => ("unknown".to_string(), false),
    };
    let data = RobotInspectData {
        host: opts.node.clone(),
        certificate_name: p.certificate_name().to_string(),
        addresses: p.addresses().iter().map(ToString::to_string).collect(),
        port: opts.port,
        is_derp_relayed: is_derp,
        state: "discovered".into(),
        // Neither is known without a host session; see `fr displays`.
        displays_count: None,
        requires_approval: None,
        transport_path,
    };
    let envelope = RobotEnvelope::success(now_unix_ms(), AcknowledgementStage::Observed, data);
    if json {
        envelope
            .render_json()
            .map_err(|_| failure("serialization_error", "Failed to render inspect JSON."))
    } else {
        Ok(envelope.render_human(
            envelope
                .data
                .as_ref()
                .map(RobotInspectData::render_human)
                .as_deref(),
        ))
    }
}

/// Exact identity only: a prefix could select a different machine.
fn exact_match(peer: &fr_tailnet::DiscoveredPeer, opts: &InspectOptions) -> bool {
    if opts.by_name {
        peer.certificate_name() == opts.node
    } else {
        peer.stable_id() == opts.node
    }
}

/// This is a local connection outcome, not a remote input-effect receipt.
/// The original foreground command retains any host closing/lease reports.
#[derive(serde::Serialize)]
struct LocalCloseData {
    scope: &'static str,
    connection_id: String,
    target: String,
    closed: bool,
    local_cleanup_confirmed: bool,
}

/// Presence describes only the authenticated local invocation. It may still be
/// dialing, awaiting consent, or reconnecting. A public instance is not a lease.
#[derive(serde::Serialize)]
struct LocalSessionData {
    scope: &'static str,
    connection_id: String,
    target: String,
    local_owner_present: bool,
    remote_session_state: Option<&'static str>,
    remote_control_granted: Option<bool>,
}

fn render_local_status(info: session_control::Info, json: bool) -> Result<String, Failure> {
    let human = (!json).then(|| {
        format!(
            "Identified the existing local foreground connection.\nTarget: {}\nConnection ID: {}\nRemote session and control state: not observed.\n",
            info.target, info.instance,
        )
    });
    let data = LocalSessionData {
        scope: "local_connection",
        connection_id: info.instance,
        target: info.target,
        local_owner_present: true,
        remote_session_state: None,
        remote_control_granted: None,
    };
    let envelope = RobotEnvelope::success(now_unix_ms(), AcknowledgementStage::Observed, data);
    if json {
        envelope.render_json().map_err(|_| {
            failure(
                "serialization_error",
                "Failed to render local session status JSON.",
            )
        })
    } else {
        Ok(envelope.render_human(human.as_deref()))
    }
}

pub(super) fn run_disconnect(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    opts: &DisconnectOptions,
    json: bool,
) -> Result<String, Failure> {
    close(runtime, cx, shutdown, stopped, &opts.node, None, json)
}

pub(super) fn run_robot(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    command: &RobotCommand,
    json: bool,
) -> Result<String, Failure> {
    if let RobotCommand::SessionStatus { node } = command {
        let info = runtime
            .block_on(shutdown.run(cx, stopped, session_control::probe(cx, node)))
            .map_err(local_failure)?;
        return render_local_status(info, json);
    }
    if let RobotCommand::SessionClose(options) = command {
        if options.lease.is_some() {
            return Err(Failure::new(
                "local_lease_unavailable",
                "This foreground connection exposes no robot lease handle. Omit --lease to close the exact local target; a supplied handle is never ignored or treated as authority.",
                2,
            ));
        }
        return close(
            runtime,
            cx,
            shutdown,
            stopped,
            &options.node,
            options.connection_id.as_deref(),
            json,
        );
    }
    Err(Failure::new(
        "robot_surface_unavailable",
        "Robot session-open, observe and input are not backed by a live session yet. Use fr connect for a foreground session; fr robot session status identifies that local invocation, and session close can stop it.",
        2,
    ))
}

fn close(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    target: &str,
    expected: Option<&str>,
    json: bool,
) -> Result<String, Failure> {
    let request = async {
        match expected {
            Some(instance) => session_control::stop_if(cx, target, instance).await,
            None => session_control::stop(cx, target).await,
        }
    };
    let result = runtime
        .block_on(shutdown.run(cx, stopped, request))
        .map_err(local_failure)?;
    if result.completion != session_control::Completion::Confirmed {
        return Err(failure(
            "native_cleanup_incomplete",
            "The original local connection stopped, but did not confirm all native cleanup. Inspect its result and remaining workers before starting another connection; prior effects are not rolled back.",
        ).uncertain());
    }
    let data = LocalCloseData {
        scope: "local_connection",
        connection_id: result.info.instance,
        target: result.info.target,
        closed: true,
        local_cleanup_confirmed: true,
    };
    let envelope = RobotEnvelope::success(now_unix_ms(), AcknowledgementStage::Observed, data);
    if json {
        envelope.render_json().map_err(|_| {
            failure(
                "serialization_error",
                "Failed to render local closure JSON.",
            )
        })
    } else {
        Ok(envelope.render_human(Some(
            "The original local connection has stopped and native cleanup is confirmed. Remote key release and prior effects are reported separately by the original fr connect command.\n",
        )))
    }
}

pub(super) fn local_failure(error: session_control::Error) -> Failure {
    use session_control::Error as E;
    let (code, next, exit) = match error {
        E::InvalidTarget => ("invalid_local_target", "Use the exact nonempty target supplied to the running fr connect command; no prefixes or aliases are inferred.", 2),
        E::InvalidInstance => ("invalid_connection_id", "Use the complete 32-character lowercase hexadecimal connection_id from fr robot session status; it is a comparison identifier, not an input lease.", 2),
        E::AlreadyRunning => ("local_session_already_running", "A foreground connection already owns this exact local target. Use fr disconnect with that target, or its window, before connecting again.", 2),
        E::NoSession => ("no_local_session", "No running connection for this exact target was found for this user. Use the same target text as fr connect; nothing was disconnected.", 2),
        E::UntrustedPeer => ("local_session_untrusted", "The local endpoint did not belong to the same operating-system user. No local control request was authorized.", 1),
        E::WrongOwner => ("local_session_changed", "The local endpoint did not identify the expected target and connection. No stop request was sent. Inspect the original fr connect process before trying again.", 1),
        E::Protocol => ("local_session_protocol", "The local connection returned an invalid control record. Check that both commands use compatible FrankenRemote versions.", 1),
        E::Timeout => ("local_session_timeout", "The original local connection did not complete its control handshake in time. Check its foreground process; no replacement connection was opened.", 1),
        E::OutcomeUnknown => return failure(
            "disconnect_outcome_unknown",
            "The stop request may have reached the original connection, but its cleanup reply was not collected. Inspect the original fr connect result before any new connection; this command does not retry or signal another process.",
        ).uncertain(),
        E::Cancelled => ("cancelled", "Local connection lookup was cancelled before a stop request was submitted.", 130),
        E::Unavailable => ("local_session_unavailable", "The local connection control endpoint is unavailable. Use the original viewer window or Ctrl-C to stop it.", 1),
    };
    Failure::new(code, next, exit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspect_matches_exact_stable_id_or_canonical_name_never_a_prefix() {
        let peer = fr_tailnet::DiscoveredPeer::from_parts(
            "n-host".into(),
            "workstation.fixture.ts.net".into(),
            vec!["100.64.0.2".parse().unwrap()],
            fr_tailnet::PeerTransportPath::Unknown,
        );
        let opts = |node: &str, by_name| InspectOptions {
            node: node.into(),
            by_name,
            port: 8443,
        };
        assert!(exact_match(&peer, &opts("n-host", false)));
        assert!(exact_match(
            &peer,
            &opts("workstation.fixture.ts.net", true)
        ));
        for (node, by_name) in [
            ("n-hos", false),
            ("workstation.fixture.ts.net", false),
            ("workstation", true),
            ("workstation.fixture", true),
            ("n-host", true),
        ] {
            assert!(
                !exact_match(&peer, &opts(node, by_name)),
                "{node} {by_name}"
            );
        }
    }

    #[test]
    fn local_status_reports_identity_without_manufacturing_remote_readiness() {
        let instance = "0123456789abcdef0123456789abcdef";
        for json in [true, false] {
            let text = render_local_status(
                session_control::Info {
                    target: "n-host".into(),
                    instance: instance.into(),
                },
                json,
            )
            .unwrap();
            assert!(text.contains(instance));
            if json {
                let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                let data = &value["data"];
                assert_eq!(data["scope"], "local_connection");
                assert_eq!(data["target"], "n-host");
                assert_eq!(data["connection_id"], instance);
                assert_eq!(data["local_owner_present"], true);
                assert_eq!(data.get("remote_session_state"), Some(&serde_json::Value::Null));
                assert_eq!(data.get("remote_control_granted"), Some(&serde_json::Value::Null));
                assert!(data.get("lease").is_none());
                assert!(data.get("local_cleanup_confirmed").is_none());
            } else {
                assert!(text.contains("not observed"));
            }
        }
    }

    #[test]
    fn stale_pins_and_uncertain_stops_keep_different_failure_codes() {
        assert_eq!(
            local_failure(session_control::Error::InvalidInstance).code,
            "invalid_connection_id"
        );
        assert_eq!(
            local_failure(session_control::Error::WrongOwner).code,
            "local_session_changed"
        );
        assert_eq!(
            local_failure(session_control::Error::OutcomeUnknown).code,
            "disconnect_outcome_unknown"
        );
    }
}
