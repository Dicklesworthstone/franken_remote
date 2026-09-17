#![forbid(unsafe_code)]
//! Native CLI entry point. Hosting is never enabled by installing/running it.
#[cfg(target_os = "linux")]
#[path = "fr_cli/linux.rs"]
mod linux;
#[cfg(target_os = "linux")]
#[path = "fr_cli/options.rs"]
mod options;
#[path = "fr_cli/output.rs"]
mod output;
use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Failure {
    code: &'static str,
    next: &'static str,
    exit: u8,
}
impl Failure {
    const fn new(code: &'static str, next: &'static str, exit: u8) -> Self {
        Self { code, next, exit }
    }
}
#[cfg(target_os = "linux")]
const HELP: &str = "FrankenRemote development client\n\nfr hosts [--json] [--socket /absolute/tailscaled.sock]\nfr connect NODE_ID --view-only --experimental-native --display HANDLE\n    --worker /absolute/fr-media-worker --trust-roots /absolute/local-ca-roots.pem\n    [--by-name] [--x-display :0] [--socket /absolute/tailscaled.sock]\n    [--port 8443] [--ipv6] [--attempts 1..32] [--json]\n\nDiscovery lists machines, not installed/ready desktops or access permissions.\nConnect uses fresh installed-tailnet identity and strict TLS on every attempt.\n--by-name selects an exact canonical tailnet FQDN, never arbitrary DNS or URLs.\nNative transport/media remain unqualified. Control, clipboard, audio and files\nare NOT enabled by this view-only command. Set XAUTHORITY in the local environment\nwhen the window and worker require it. Close the window or use Ctrl-C to stop.\n";
#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    let mut args = Vec::new();
    let mut json = false;
    for arg in std::env::args_os().skip(1) {
        if arg == "--json" {
            json = true;
        }
        if args.len() == 32 || arg.len() > 4096 {
            return finish(
                Err(Failure::new(
                    "invalid_arguments",
                    "Run fr --help; argument limits exceeded.",
                    2,
                )),
                json,
            );
        }
        let Ok(arg) = arg.into_string() else {
            return finish(
                Err(Failure::new("invalid_arguments", "Use UTF-8 arguments.", 2)),
                json,
            );
        };
        args.push(arg);
    }
    let parsed = match options::parse(&args) {
        Ok(v) => v,
        Err(e) => return finish(Err(e), json),
    };
    if matches!(parsed.command, options::Command::Help) {
        return if output::write(HELP).is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(74)
        };
    }
    let result = linux::run(&parsed);
    finish(result, parsed.json)
}
#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    let json = std::env::args_os()
        .skip(1)
        .take(32)
        .any(|arg| arg == "--json");
    finish(
        Err(Failure::new(
            "platform_unavailable",
            "This executable currently implements only the Linux installed-tailnet/X11 path.",
            2,
        )),
        json,
    )
}
fn finish(result: Result<String, Failure>, json: bool) -> ExitCode {
    let (text, code) = match result {
        Ok(text) => (text, 0),
        Err(failure) => (output::failure(failure, json), failure.exit),
    };
    if output::write(&text).is_err() {
        ExitCode::from(74)
    } else {
        ExitCode::from(code)
    }
}
