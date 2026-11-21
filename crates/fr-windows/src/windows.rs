pub mod app;
mod hosts;
mod install;
mod probe;
mod receive_folder;
mod render;
mod viewer;

use crate::{
    USAGE,
    options::{ConnectionCommand, Decoder, DisplayChoice, Family, ReceiveConflict},
};
use fr_mobile_ffi::directory::{Request, fetch};
use fr_mobile_ffi::session::{
    Config,
    connection::{self, AddressFamily, Target},
};
use fr_tailnet::{DEFAULT_SERVICE_PORT, Error, PeerTransportPath};
use std::{io::Write, process::ExitCode};

#[allow(
    clippy::too_many_lines,
    reason = "one command line's ordered selection and session setup"
)]
pub fn run(args: &[String]) -> ExitCode {
    if let [command, rest @ ..] = args
        && command == "app"
    {
        return app::launch(rest);
    }
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
    fr_mobile_ffi::video::set_decoder_preference(match options.decoder {
        Decoder::Automatic => fr_mobile_ffi::video::DecoderPreference::Automatic,
        Decoder::Hardware => fr_mobile_ffi::video::DecoderPreference::Hardware,
        Decoder::Software => fr_mobile_ffi::video::DecoderPreference::Software,
    });
    let family = match options.family {
        Family::Automatic => AddressFamily::Automatic,
        Family::Ipv4 => AddressFamily::Ipv4,
        Family::Ipv6 => AddressFamily::Ipv6,
    };
    let selected = if let Some(selection) = selection {
        let snapshot = match connection::discover() {
            Ok(snapshot) => snapshot,
            Err(error) => return discovery_failed(error),
        };
        Target::installed(&snapshot, selection, options.addresses, family)
    } else {
        let request = hosts::Request {
            mode: hosts::Mode::Command,
            port: options.port,
            settings: crate::settings::Settings::default(),
            notice: None,
            support: None,
            offer_install: false,
        };
        match hosts::pick(&request) {
            Ok(hosts::Picked::Machine(chosen)) => {
                Target::selected(&chosen.snapshot, &chosen.peer, options.addresses, family)
            }
            Ok(hosts::Picked::Closed | hosts::Picked::Install) => return ExitCode::SUCCESS,
            Err(hosts::PickError::Discovery(error)) => return discovery_failed(error),
            Err(hosts::PickError::Choice(error)) => return choice_failed(error),
        }
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
    let download = match select_download(options.receive.as_ref()) {
        Ok(download) => download,
        Err(error) => {
            eprintln!(
                "fr-windows: {}. Select an existing absolute local directory owned by you, without links or other writers, and valid receive limits; no remote session was opened.",
                error.code(),
            );
            return ExitCode::from(2);
        }
    };
    let upload = if !options.send.is_empty() {
        match fr_mobile_ffi::session::Upload::select_many(&options.send) {
            Ok(upload) => Some(upload),
            Err(error) => {
                eprintln!(
                    "fr-windows: file_selection_refused ({error:?}). Select up to eight distinct regular files; no remote session was opened."
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
        download,
        picture: None,
        display: match options.display {
            DisplayChoice::Primary => fr_mobile_ffi::session::DisplayChoice::Primary,
            DisplayChoice::Only => fr_mobile_ffi::session::DisplayChoice::Only,
            DisplayChoice::Choose => fr_mobile_ffi::session::DisplayChoice::Choose,
            DisplayChoice::Handle(handle) => fr_mobile_ffi::session::DisplayChoice::Handle(handle),
        },
    };
    let view = viewer::View {
        fullscreen: options.fullscreen,
        shortcuts: options.shortcuts,
    };
    viewer::run(config, &title, options.seconds, view)
}

fn select_download(
    receive: Option<&crate::options::ReceiveOptions>,
) -> Result<
    Option<std::sync::Arc<fr_mobile_ffi::session::Download>>,
    fr_mobile_ffi::session::DownloadRefusal,
> {
    use fr_mobile_ffi::session::{Download, DownloadConflictPolicy, DownloadLimits};
    let Some(receive) = receive else {
        return Ok(None);
    };
    let defaults = DownloadLimits::default();
    let limits = DownloadLimits {
        max_file_bytes: receive.max_file_bytes.unwrap_or(defaults.max_file_bytes),
        max_session_bytes: receive
            .max_session_bytes
            .unwrap_or(defaults.max_session_bytes),
    };
    let conflict = match receive.conflict {
        ReceiveConflict::Reject => DownloadConflictPolicy::Reject,
        ReceiveConflict::KeepBoth => DownloadConflictPolicy::KeepBoth,
    };
    Download::select_with_limits(&receive.directory, limits, conflict).map(Some)
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
                "os": peer.os(),
                "online": peer.online(),
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

static APPLICATION_SESSION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// This process is a session the desktop application started (its texts
/// name the application's choices instead of command-line flags).
pub(crate) fn application_session() -> bool {
    APPLICATION_SESSION.load(std::sync::atomic::Ordering::Relaxed)
}

/// The executable's icon (resource 1, embedded by build.rs) at the system's
/// large or small icon size; a null icon (the system default) without it.
pub(crate) fn app_icon(small: bool) -> windows::Win32::UI::WindowsAndMessaging::HICON {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, HICON, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, SM_CXICON, SM_CXSMICON,
        SM_CYICON, SM_CYSMICON,
    };
    // SAFETY: resource lookups in this module's own image; the returned
    // shared icon handle is owned by the system for the process lifetime.
    unsafe {
        let (cx, cy) = if small {
            (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON))
        } else {
            (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON))
        };
        GetModuleHandleW(None)
            .ok()
            .and_then(|module| {
                LoadImageW(
                    Some(module.into()),
                    windows::core::PCWSTR(std::ptr::without_provenance(1)),
                    IMAGE_ICON,
                    cx,
                    cy,
                    LR_DEFAULTCOLOR | windows::Win32::UI::WindowsAndMessaging::LR_SHARED,
                )
                .ok()
            })
            .map_or_else(HICON::default, |handle| HICON(handle.0))
    }
}
