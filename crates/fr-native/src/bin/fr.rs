#![forbid(unsafe_code)]
//! Native CLI entry point. Hosting is never enabled by installing/running it.
#[cfg(target_os = "linux")]
#[path = "fr_cli/linux.rs"]
mod linux;
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
const HELP: &str = "\
FrankenRemote development client

fr hosts [--json] [--socket /absolute/tailscaled.sock]
fr status [--json] [--socket /absolute/tailscaled.sock]
fr doctor [--port 8443] [--socket /absolute/tailscaled.sock] [--trust-roots /absolute/local-ca-roots.pem] [--json]
fr inspect NODE_ID [--by-name] [--port 8443] [--socket /absolute/tailscaled.sock] [--json]
fr disconnect NODE_ID [--socket /absolute/tailscaled.sock] [--json]
fr displays NODE_ID --experimental-native [--trust-roots /absolute/ca-roots.pem]
    [--by-name] [--socket /absolute/tailscaled.sock] [--port 8443] [--ipv6] [--json]
fr connect NODE_ID --view-only --experimental-native --display HANDLE|only|choose
    [--worker /absolute/fr-media-worker] [--trust-roots /absolute/ca-roots.pem]
    [--by-name] [--x-display :0] [--socket /absolute/tailscaled.sock]
    [--port 8443] [--ipv6] [--attempts 1..32] [--fit WIDTHxHEIGHT] [--json]
fr robot session open NODE_ID [--role view|control] [--port 8443] [--socket /absolute/tailscaled.sock] [--json]
fr robot session close NODE_ID [--lease LEASE] [--socket /absolute/tailscaled.sock] [--json]
fr robot observe NODE_ID [--display N] [--screenshot /path/screen.png] [--evidence-level decoded|submitted_to_compositor|instrumentally_observed] [--port 8443] [--socket /absolute/tailscaled.sock] [--json]
fr robot input NODE_ID --lease LEASE --request-id REQUEST [--batch /path/batch.json] [--precondition-geometry GEN] [--max-observation-age MS] [--precondition-lease LEASE] [--precondition-focus WINDOW] [--semantic-evidence none|unverified_pixels|adapter|instrumentation] [--json]

Doctor diagnoses installed Tailscale status, service port collisions, and certificate lifecycle.
Discovery lists machines, not installed/ready desktops or access permissions.
--trust-roots defaults to the distribution bundle /etc/ssl/certs/ca-certificates.crt;
--worker defaults to the fr-media-worker installed beside fr.
Displays requires host approval when configured; it starts no decoder or input.
--display only explicitly selects the sole current display, refusing ambiguity.
--display choose opens a native chooser in each approved connection.
--fit bounds the fixed local window and explicitly enables CPU nearest-neighbour
aspect fitting; the remote display is not resized. Omit it for native pixels.
Connect uses fresh installed-tailnet identity and strict TLS on every attempt.
--by-name selects an exact canonical tailnet FQDN, never arbitrary DNS or URLs.
Native transport/media remain unqualified. Control, clipboard, audio and files
are NOT enabled by this view-only command. Set XAUTHORITY in the local environment
when the window and worker require it. Close the window or use Ctrl-C to stop.
";

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
    #[cfg(target_os = "linux")]
    let result = linux::run(&parsed);
    #[cfg(not(target_os = "linux"))]
    let result = Err(Failure::new(
        "platform_unavailable",
        "Tailnet local authority integration in fr is currently implemented on Linux.",
        2,
    ));
    finish(result, parsed.json)
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
