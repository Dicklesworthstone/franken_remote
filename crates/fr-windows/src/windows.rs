mod hosts;
mod probe;
mod render;
mod viewer;

use crate::{
    USAGE,
    options::{ConnectionCommand, DisplayChoice, Family},
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
    let Some(ConnectionCommand {
        view,
        selection,
        options,
    }) = ConnectionCommand::parse(args, DEFAULT_SERVICE_PORT)
    else {
        return usage();
    };
    // Native setup and cleanup must not renew the original discovery budget.
    let choice_started = hosts::ticks();
    let snapshot = match connection::discover() {
        Ok(snapshot) => snapshot,
        Err(error) => return discovery_failed(error),
    };
    let family = match options.family {
        Family::Automatic => AddressFamily::Automatic,
        Family::Ipv4 => AddressFamily::Ipv4,
        Family::Ipv6 => AddressFamily::Ipv6,
    };
    let selected = if let Some(selection) = selection {
        Target::installed(&snapshot, selection, options.addresses, family)
    } else {
        let catalog = match connection::candidates(&snapshot) {
            Ok(catalog) => catalog,
            Err(error) => return discovery_failed(error),
        };
        let peer = match hosts::choose(catalog, choice_started) {
            Ok(Some(peer)) => peer,
            Ok(None) => return ExitCode::SUCCESS,
            Err(error) => return choice_failed(error),
        };
        Target::selected(&snapshot, &peer, options.addresses, family)
    };
    let target = match selected {
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
    let upload = if let Some(path) = &options.send {
        match fr_mobile_ffi::session::Upload::select(path) {
            Ok(upload) => Some(upload),
            Err(error) => {
                eprintln!(
                    "fr-windows: file_selection_refused ({error:?}). Select one regular file; no remote session was opened."
                );
                return ExitCode::from(2);
            }
        }
    } else {
        options
            .control
            .then(fr_mobile_ffi::session::Upload::interactive)
    };
    let title = format!("FrankenRemote - {}", target.certificate_name());
    let config = Config {
        target,
        port: options.port,
        control: options.control,
        clipboard: options.control && options.clipboard,
        upload,
        picture: None,
        display: match options.display {
            DisplayChoice::Primary => fr_mobile_ffi::session::DisplayChoice::Primary,
            DisplayChoice::Only => fr_mobile_ffi::session::DisplayChoice::Only,
            DisplayChoice::Choose => fr_mobile_ffi::session::DisplayChoice::Choose,
            DisplayChoice::Handle(handle) => fr_mobile_ffi::session::DisplayChoice::Handle(handle),
        },
    };
    viewer::run(config, &title, options.seconds)
}

fn usage() -> ExitCode {
    eprint!("{USAGE}");
    ExitCode::from(2)
}

fn choice_failed(error: hosts::Error) -> ExitCode {
    let guidance = match error {
        hosts::Error::Expired => {
            "The original Tailscale snapshot expired. Run the command again to refresh it."
        }
        hosts::Error::StaleEvent
        | hosts::Error::InvalidChoice
        | hosts::Error::ClockChanged
        | hosts::Error::EventFlood => "Run the command again and choose a current machine.",
        hosts::Error::CleanupIncomplete => {
            "The picker could not confirm that its window closed. Close the application before retrying."
        }
        _ => "Run the command again, or use 'fr-windows hosts' and supply an explicit destination.",
    };
    eprintln!("fr-windows: machine choice refused ({error:?}). {guidance}");
    ExitCode::from(1)
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
    let json = fetch(&Request { target, port });
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
