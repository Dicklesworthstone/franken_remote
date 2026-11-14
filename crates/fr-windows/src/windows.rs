mod probe;
mod render;
mod viewer;

use crate::{
    USAGE,
    options::{Family, Options},
};
use fr_mobile_ffi::directory::{Request, fetch};
use fr_mobile_ffi::session::{
    Config,
    connection::{self, AddressFamily, Target},
};
use fr_tailnet::{DEFAULT_SERVICE_PORT, Error, PeerTransportPath};
use std::{io::Write, process::ExitCode};

pub fn run(args: &[String]) -> ExitCode {
    if let [command, rest @ ..] = args
        && command == "capture-probe"
    {
        return probe::run(rest);
    }
    if let [command] = args
        && command == "hosts"
    {
        return hosts();
    }
    let [command, selection, rest @ ..] = args else {
        return usage();
    };
    let view = match command.as_str() {
        "directory" => false,
        "view" => true,
        _ => return usage(),
    };
    let Some(options) = Options::parse(rest, view, DEFAULT_SERVICE_PORT) else {
        return usage();
    };
    let snapshot = match connection::discover() {
        Ok(snapshot) => snapshot,
        Err(error) => return discovery_failed(error),
    };
    let family = match options.family {
        Family::Automatic => AddressFamily::Automatic,
        Family::Ipv4 => AddressFamily::Ipv4,
        Family::Ipv6 => AddressFamily::Ipv6,
    };
    let target = match Target::installed(&snapshot, selection, options.addresses, family) {
        Ok(target) => target,
        Err(error) => {
            let guidance = if error == Error::AddressMismatch {
                "Use the complete address set from 'fr-windows hosts', or omit --address and use --ipv4 or --ipv6."
            } else {
                "Run 'fr-windows hosts' and choose one stable_id or full name from that listing."
            };
            eprintln!("fr-windows: host selection refused ({error:?}). {guidance}");
            return ExitCode::from(2);
        }
    };
    if !view {
        return directory(target, options.port);
    }
    let title = format!("FrankenRemote - {}", target.certificate_name());
    let config = Config {
        target,
        port: options.port,
        control: options.control,
        picture: None,
    };
    viewer::run(config, &title, options.seconds)
}

fn usage() -> ExitCode {
    eprint!("{USAGE}");
    ExitCode::from(2)
}

fn discovery_failed(error: Error) -> ExitCode {
    let guidance = match error {
        Error::BackendNotRunning => "Connect this machine to Tailscale and retry.",
        Error::LocalApiUnavailable => "Start the installed Tailscale service and retry.",
        Error::LocalApiDenied | Error::UntrustedLocalApi => {
            "Check access to the installed Tailscale service's authenticated LocalAPI."
        }
        Error::Timeout => "The installed Tailscale service did not answer in time; retry.",
        _ => "Check the installed Tailscale service and retry 'fr-windows hosts'.",
    };
    eprintln!("fr-windows: local discovery failed ({error:?}). {guidance}");
    ExitCode::from(1)
}

/// One local informational snapshot. Selecting a listed node later repeats
/// discovery; the connector then resolves the retained ID/name before dialing.
fn hosts() -> ExitCode {
    let snapshot = match connection::discover() {
        Ok(snapshot) => snapshot,
        Err(error) => return discovery_failed(error),
    };
    let machines: Vec<_> = snapshot
        .peers()
        .iter()
        .map(|peer| {
            let path = match peer.transport_path() {
                PeerTransportPath::Direct { .. } => "direct",
                PeerTransportPath::DerpRelayed { .. } => "relayed",
                PeerTransportPath::Unknown => "unknown",
            };
            serde_json::json!({
                "stable_id": peer.stable_id(),
                "name": peer.certificate_name(),
                "addresses": peer.addresses().iter().map(ToString::to_string).collect::<Vec<_>>(),
                "path": path,
            })
        })
        .collect();
    let excluded = snapshot.excluded();
    let document = serde_json::json!({
        "status": "ok",
        "source": "installed_tailscale",
        "readiness": "not_probed",
        "machines": machines,
        "excluded": {
            "shared": excluded.shared,
            "expired": excluded.expired,
            "unusable": excluded.unusable,
        },
    });
    if writeln!(std::io::stdout().lock(), "{document}").is_err() {
        return ExitCode::from(74);
    }
    ExitCode::SUCCESS
}

/// One fetch, printed as the session core's JSON document. Exit 0 only when
/// the host listed its directory.
fn directory(target: Target, port: u16) -> ExitCode {
    let json = fetch(Request { target, port });
    let listed =
        serde_json::from_str::<serde_json::Value>(&json).is_ok_and(|value| value["status"] == "ok");
    if writeln!(std::io::stdout().lock(), "{json}").is_err() {
        return ExitCode::from(74);
    }
    if listed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
