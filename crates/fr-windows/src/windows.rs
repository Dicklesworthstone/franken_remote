mod render;
mod viewer;

use crate::USAGE;
use fr_mobile_ffi::directory::{Request, fetch};
use fr_mobile_ffi::session::Config;
use fr_tailnet::{DEFAULT_SERVICE_PORT, HostName, HostPin};
use std::{io::Write, net::IpAddr, process::ExitCode};

pub fn run(args: &[String]) -> ExitCode {
    let [command, host, rest @ ..] = args else {
        return usage();
    };
    let view = match command.as_str() {
        "directory" => false,
        "view" => true,
        _ => return usage(),
    };
    let Some(options) = Options::parse(rest, view) else {
        return usage();
    };
    let Ok(name) = HostName::parse(host) else {
        eprintln!("fr-windows: the host must be an exact <machine>.<tailnet>.ts.net name");
        return ExitCode::from(2);
    };
    let pin = if options.addresses.is_empty() {
        None
    } else if let Ok(pin) = HostPin::from_addresses(name.clone(), options.addresses) {
        Some(pin)
    } else {
        eprintln!("fr-windows: --address must be the host's own tailnet address");
        return ExitCode::from(2);
    };
    if !view {
        return directory(name, options.port, pin);
    }
    let direct = pin.is_some();
    let config = Config {
        host: name,
        port: options.port,
        pin,
        control: options.control,
        direct,
        picture: None,
    };
    viewer::run(config, &format!("FrankenRemote - {host}"), options.seconds)
}

fn usage() -> ExitCode {
    eprint!("{USAGE}");
    ExitCode::from(2)
}

struct Options {
    port: u16,
    /// `--address IP` (repeatable): dial these tailnet addresses instead of
    /// resolving the name. Windows' `MagicDNS` answers only the A record, so
    /// a host that serves IPv6 alone is reachable only this way until the
    /// client reads the installed `LocalAPI`.
    addresses: Vec<IpAddr>,
    /// `view --control`: ask the host for control at once.
    control: bool,
    /// `view --seconds N`: close in order after N seconds.
    seconds: Option<u64>,
}
impl Options {
    fn parse(mut rest: &[String], view: bool) -> Option<Self> {
        let mut options = Self {
            port: DEFAULT_SERVICE_PORT,
            addresses: Vec::new(),
            control: false,
            seconds: None,
        };
        loop {
            match rest {
                [] => return Some(options),
                [flag, tail @ ..] if view && flag == "--control" => {
                    options.control = true;
                    rest = tail;
                }
                [flag, value, tail @ ..] => {
                    match flag.as_str() {
                        "--port" => options.port = value.parse().ok().filter(|port| *port != 0)?,
                        "--address" => options.addresses.push(value.parse().ok()?),
                        "--seconds" if view => {
                            options.seconds = Some(value.parse().ok().filter(|s| *s != 0)?);
                        }
                        _ => return None,
                    }
                    rest = tail;
                }
                [_] => return None,
            }
        }
    }
}

/// One fetch, printed as the session core's JSON document. Exit 0 only when
/// the host listed its directory.
fn directory(host: HostName, port: u16, pin: Option<HostPin>) -> ExitCode {
    let direct = pin.is_some();
    let json = fetch(Request {
        host,
        port,
        pin,
        direct,
    });
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
