//! `fr status`, `fr inspect`, `fr disconnect` and `fr robot`.
//!
//! Status and inspect report only what the installed Tailscale `LocalAPI`
//! returns; anything not probed is unknown, never a plausible default. `fr`
//! keeps no background sessions, so disconnect and the robot surface (plan 18,
//! bead `fr-agent-robot-surface-3o3`) refuse until a live client session backs
//! them: no session is opened, nothing is observed and no input is sent.

use super::super::options::{DisconnectOptions, InspectOptions, RobotCommand};
use super::{Cell, Cx, Failure, LocalApi, Runtime, Shutdown, failure, tailnet};
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
    // Exact identity only: a prefix could select a different machine.
    let peer = snapshot.peers().iter().find(|p| {
        if opts.by_name {
            p.certificate_name() == opts.node
        } else {
            p.stable_id() == opts.node
        }
    });
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

pub(super) fn run_disconnect(_opts: &DisconnectOptions) -> Failure {
    Failure::new(
        "no_background_session",
        "fr keeps no background sessions: a `fr connect` session runs in the foreground and ends when its window closes or fr is interrupted (Ctrl-C). Nothing was disconnected.",
        2,
    )
}

pub(super) fn run_robot(_cmd: &RobotCommand) -> Failure {
    Failure::new(
        "robot_surface_unavailable",
        "The robot surface is not backed by a live session yet: no session was opened, nothing was observed, no file was written and no input was sent. Use `fr displays` or `fr connect --view-only`.",
        2,
    )
}
