//! Fresh signed macOS destination identity, without a remote capability probe.
//! The portable helpers cannot open a runtime or invoke a native process.
use super::{
    Failure,
    options::{Command, Options},
};
use fr_client::robot::{AcknowledgementStage, RobotEnvelope, RobotInspectData};
use fr_tailnet::{PeerSelector, PeerTransportPath};
use std::net::IpAddr;

#[derive(Debug)]
struct Request<'a> {
    selector: PeerSelector<'a>,
    port: u16,
}

fn prepare(options: &Options) -> Result<Request<'_>, Failure> {
    let Command::Inspect(inspect) = &options.command else {
        return Err(Failure::new(
            "invalid_arguments",
            "Use fr inspect NODE [--by-name] [--port N] [--json].",
            2,
        ));
    };
    if inspect.saved.is_some() {
        return Err(Failure::new(
            "saved_hosts_unsupported",
            "macOS inspection requires a stable node ID or --by-name canonical name. Omit --saved and --hosts-file; use fr hosts to select the machine.",
            2,
        ));
    }
    if inspect.probe.is_some() {
        return Err(Failure::new(
            "platform_unavailable",
            "Remote capability probes need the macos-desktop feature. Omit --probe and its transport flags for installed Tailscale identity inspection.",
            2,
        ));
    }
    Ok(Request {
        selector: if inspect.by_name {
            PeerSelector::Name(&inspect.node)
        } else {
            PeerSelector::StableId(&inspect.node)
        },
        port: inspect.port,
    })
}

#[cfg(target_os = "macos")]
pub(super) fn run(options: &Options) -> Result<String, Failure> {
    use super::{
        discovery::tailnet,
        runtime::{self, Shutdown},
    };
    use std::{cell::Cell, time::SystemTime};

    // Refuse unsupported modes before signals, runtime creation or a native
    // process. They must never silently become an ordinary identity lookup.
    let request = prepare(options)?;
    let mut shutdown = Shutdown::new()?;
    let (runtime, cx) = runtime::create()?;
    let stopped = Cell::new(false);
    // --socket names one root tailscaled socket, as on Linux.
    let api = match &options.socket {
        Some(path) => fr_tailnet::LocalApi::new(path).map_err(tailnet)?,
        None => fr_tailnet::LocalApi::installed(),
    };
    let target = runtime
        .block_on(shutdown.run(&cx, &stopped, api.peer_target(&cx, request.selector)))
        .map_err(tailnet)?;
    if stopped.get() {
        return Err(tailnet(fr_tailnet::Error::Cancelled));
    }
    // The output is only a snapshot. Check the actual target's original
    // lifetime immediately before collecting its authenticated identity.
    api.check_peer_target(&cx, &target).map_err(tailnet)?;
    let data = identity(
        target.stable_id(),
        target.certificate_name(),
        target.addresses(),
        request.port,
        target.transport_path(),
    );
    let timestamp_ms = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .ok_or_else(|| tailnet(fr_tailnet::Error::Clock))?;
    let text = render(data, timestamp_ms, options.json)?;
    api.check_peer_target(&cx, &target).map_err(tailnet)?;
    Ok(text)
}

fn identity(
    stable_id: &str,
    certificate_name: &str,
    addresses: &[IpAddr],
    port: u16,
    path: PeerTransportPath,
) -> RobotInspectData {
    let (transport_path, is_derp_relayed) = match path {
        PeerTransportPath::Direct { cur_addr } => (format!("direct ({cur_addr})"), false),
        PeerTransportPath::DerpRelayed { relay } => (format!("derp_relayed ({relay})"), true),
        PeerTransportPath::Unknown => ("unknown".into(), false),
    };
    RobotInspectData {
        host: stable_id.into(),
        certificate_name: certificate_name.into(),
        addresses: addresses.iter().map(ToString::to_string).collect(),
        port,
        is_derp_relayed,
        state: "identity_checked".into(),
        displays_count: None,
        requires_approval: None,
        transport_path,
    }
}

fn render(mut data: RobotInspectData, timestamp_ms: u64, json: bool) -> Result<String, Failure> {
    if !json {
        // The shared inspection formatter prints fields verbatim. Preserve
        // opaque metadata in JSON, but escape terminal controls for people.
        data.host = data.host.escape_default().to_string();
        data.certificate_name = data.certificate_name.escape_default().to_string();
        data.transport_path = data.transport_path.escape_default().to_string();
    }
    let envelope = RobotEnvelope::success(timestamp_ms, AcknowledgementStage::Observed, data);
    if json {
        envelope
            .render_json()
            .map_err(|_| Failure::new("serialization_error", "Failed to render inspect JSON.", 1))
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

#[cfg(test)]
#[path = "macos_inspect/tests.rs"]
mod tests;
