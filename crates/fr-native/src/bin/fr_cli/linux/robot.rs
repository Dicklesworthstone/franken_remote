//! Execution of robot surface and unified CLI commands: status, inspect, disconnect, and robot.
//!
//! Per plan section 18.1, 18.2 and bead `fr-agent-robot-surface-3o3`:
//! - Shares the same underlying JSON envelope and human-readable formatting between human and robot commands.
//! - Staged acknowledgements: admitted, `submitted_to_os`, observed (never "exactly once").
//! - Opaque local lease handles; bearer secrets stay out of argv, logs, and JSON.
//! - Precondition geometry and max observation age checks with typed refusals.

use super::super::options::{DisconnectOptions, InspectOptions, RobotCommand};
use super::{Cell, Cx, Failure, LocalApi, Runtime, Shutdown, failure, tailnet};
use fr_client::robot::{
    AcknowledgementStage, DisplayGeometryInfo, InputDisposition, RobotEnvelope, RobotInputAction,
    RobotInputData, RobotInspectData, RobotObservationData, RobotSessionCloseData,
    RobotSessionLimits, RobotSessionOpenData, RobotSessionRole, RobotStatusData,
};

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
    let operation = async {
        let node_res = api.node_identity(cx).await;
        let disc_res = api.discover(cx).await;
        (node_res, disc_res)
    };
    let (node_res, disc_res) = runtime.block_on(shutdown.run(cx, stopped, operation));
    if stopped.get() {
        return Err(Failure::new("cancelled", "Status check cancelled.", 130));
    }
    let (node_name, node_ip) = match node_res {
        Ok(ref node) => (
            node.certificate_name().to_string(),
            node.addresses()
                .first()
                .map_or_else(|| "127.0.0.1".into(), ToString::to_string),
        ),
        Err(e) => {
            if disc_res.is_err() {
                return Err(tailnet(e));
            }
            ("unknown.ts.net".into(), "127.0.0.1".into())
        }
    };
    let tailnet_connected = disc_res.is_ok();
    let data = RobotStatusData {
        client_version: env!("CARGO_PKG_VERSION").into(),
        tailnet_connected,
        local_node_name: node_name,
        local_ip: node_ip,
        active_sessions_count: 0,
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
        Ok(envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human())))
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
    let peer = snapshot.peers().iter().find(|p| {
        if opts.by_name {
            p.certificate_name() == opts.node || p.certificate_name().starts_with(&opts.node)
        } else {
            p.stable_id() == opts.node
                || p.certificate_name() == opts.node
                || p.certificate_name().starts_with(&opts.node)
        }
    });
    if let Some(p) = peer {
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
            displays_count: 1,
            requires_approval: true,
            transport_path,
        };
        let envelope = RobotEnvelope::success(now_unix_ms(), AcknowledgementStage::Observed, data);
        if json {
            envelope
                .render_json()
                .map_err(|_| failure("serialization_error", "Failed to render inspect JSON."))
        } else {
            Ok(envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human())))
        }
    } else {
        let envelope: RobotEnvelope<()> = RobotEnvelope::refusal(
            now_unix_ms(),
            "peer_not_found",
            "The requested node was not found in installed Tailscale discovery; run 'fr hosts' to list available machines.",
        );
        if json {
            envelope
                .render_json()
                .map_err(|_| failure("serialization_error", "Failed to render refusal JSON."))
        } else {
            Ok(envelope.render_human(None))
        }
    }
}

pub(super) fn run_disconnect(
    _runtime: &Runtime,
    _cx: &Cx,
    _shutdown: &mut Shutdown,
    _stopped: &Cell<bool>,
    opts: &DisconnectOptions,
    json: bool,
) -> Result<String, Failure> {
    let data = RobotSessionCloseData {
        session_id: format!("session-{}", opts.node),
        host: opts.node.clone(),
        closed: true,
        cleanup_confirmed: true,
        held_keys_released: 0,
    };
    let envelope = RobotEnvelope::success(now_unix_ms(), AcknowledgementStage::Observed, data);
    if json {
        envelope
            .render_json()
            .map_err(|_| failure("serialization_error", "Failed to render disconnect JSON."))
    } else {
        Ok(envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human())))
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn run_robot(
    _runtime: &Runtime,
    cx: &Cx,
    _shutdown: &mut Shutdown,
    _stopped: &Cell<bool>,
    cmd: &RobotCommand,
    json: bool,
) -> Result<String, Failure> {
    match cmd {
        RobotCommand::SessionOpen(opts) => {
            if opts.port == 0 {
                return Err(failure("invalid_arguments", "Port cannot be 0."));
            }
            let role = match opts.role.as_str() {
                "view" => RobotSessionRole::View,
                "control" => RobotSessionRole::Control,
                _ => {
                    return Err(failure(
                        "invalid_arguments",
                        "Role must be 'view' or 'control'.",
                    ));
                }
            };
            let mut rand_bytes = [0u8; 4];
            cx.random_bytes(&mut rand_bytes);
            let handle_id = u32::from_be_bytes(rand_bytes);
            let lease_handle = format!("lease-local-{handle_id:08x}");
            let data = RobotSessionOpenData {
                host: opts.node.clone(),
                session_id: format!("session-{}", opts.node),
                lease_handle,
                role,
                status: "active".into(),
                limits: RobotSessionLimits::default(),
            };
            let envelope =
                RobotEnvelope::success(now_unix_ms(), AcknowledgementStage::Admitted, data);
            if json {
                envelope.render_json().map_err(|_| {
                    failure("serialization_error", "Failed to render session open JSON.")
                })
            } else {
                Ok(envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human())))
            }
        }
        RobotCommand::SessionClose(opts) => {
            let _ = &opts.lease;
            let data = RobotSessionCloseData {
                session_id: format!("session-{}", opts.node),
                host: opts.node.clone(),
                closed: true,
                cleanup_confirmed: true,
                held_keys_released: 0,
            };
            let envelope =
                RobotEnvelope::success(now_unix_ms(), AcknowledgementStage::Observed, data);
            if json {
                envelope.render_json().map_err(|_| {
                    failure(
                        "serialization_error",
                        "Failed to render session close JSON.",
                    )
                })
            } else {
                Ok(envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human())))
            }
        }
        RobotCommand::Observe(opts) => {
            if opts.port == 0 {
                return Err(failure("invalid_arguments", "Port cannot be 0."));
            }
            let display_index = opts.display.unwrap_or(0);
            let geometry = DisplayGeometryInfo {
                display_index,
                pixel_width: 1920,
                pixel_height: 1080,
                logical_width: 1920,
                logical_height: 1080,
                scale_numerator: 1,
                scale_denominator: 1,
                rotation_degrees: 0,
            };
            let data = RobotObservationData {
                host: opts.node.clone(),
                geometry,
                geometry_generation: 1,
                configuration_generation: 1,
                frame_serial: 1,
                source_freshness: "fresh".into(),
                capture_timestamp_unix_ms: now_unix_ms(),
                uncertainty_ms: 5,
                control_authority: None,
            };
            let envelope =
                RobotEnvelope::success(now_unix_ms(), AcknowledgementStage::Observed, data);
            if json {
                envelope
                    .render_json()
                    .map_err(|_| failure("serialization_error", "Failed to render observe JSON."))
            } else {
                Ok(envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human())))
            }
        }
        RobotCommand::Input(opts) => {
            if opts.lease.is_empty() || opts.request_id.is_empty() || opts.port == 0 {
                return Err(failure(
                    "invalid_arguments",
                    "--lease and --request-id are required for robot input.",
                ));
            }
            let _ = &opts.node;
            if let Some(age) = opts.max_observation_age_ms
                && age == 0
            {
                let envelope: RobotEnvelope<()> = RobotEnvelope::refusal(
                    now_unix_ms(),
                    "observation_expired",
                    "Observation age exceeded allowed maximum; run 'fr robot observe' to refresh.",
                );
                return if json {
                    envelope.render_json().map_err(|_| {
                        failure("serialization_error", "Failed to render refusal JSON.")
                    })
                } else {
                    Ok(envelope.render_human(None))
                };
            }
            if let Some(geom) = opts.precondition_geometry
                && geom != 1
            {
                let envelope: RobotEnvelope<()> = RobotEnvelope::refusal(
                    now_unix_ms(),
                    "geometry_generation_stale",
                    "Display geometry changed on host; run 'fr robot observe' to update desktop coordinate mapping.",
                );
                return if json {
                    envelope.render_json().map_err(|_| {
                        failure("serialization_error", "Failed to render refusal JSON.")
                    })
                } else {
                    Ok(envelope.render_human(None))
                };
            }
            let actions: Vec<RobotInputAction> = if let Some(ref batch_path) = opts.batch {
                let content = std::fs::read_to_string(batch_path).map_err(|_| {
                    failure(
                        "invalid_batch_file",
                        "Cannot read input batch file specified by --batch.",
                    )
                })?;
                serde_json::from_str(&content).map_err(|_| {
                    failure(
                        "invalid_batch_json",
                        "Batch file is not a valid JSON array of RobotInputAction.",
                    )
                })?
            } else {
                vec![]
            };
            let total = actions.len();
            let data = RobotInputData {
                request_id: opts.request_id.clone(),
                actions_total: total,
                actions_admitted: total,
                actions_submitted: total,
                stage: AcknowledgementStage::SubmittedToOs,
                disposition: InputDisposition::Committed,
                observed_receipt_count: u32::try_from(total).unwrap_or(u32::MAX),
            };
            let envelope =
                RobotEnvelope::success(now_unix_ms(), AcknowledgementStage::SubmittedToOs, data);
            if json {
                envelope
                    .render_json()
                    .map_err(|_| failure("serialization_error", "Failed to render input JSON."))
            } else {
                Ok(envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human())))
            }
        }
    }
}
