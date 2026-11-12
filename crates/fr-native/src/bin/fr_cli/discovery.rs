//! Local machine snapshots. No probe, connection, certificate or input grant.
use super::{
    Failure,
    options::Options,
    output,
    runtime::{self, Shutdown},
};
use asupersync::cx::Cx;
use fr_tailnet::{Discovery, Error as TailnetError};
use std::cell::Cell;

pub(super) fn run(options: &Options) -> Result<String, Failure> {
    let mut shutdown = Shutdown::new()?;
    let (runtime, cx) = runtime::create()?;
    let stopped = Cell::new(false);
    let snapshot = runtime
        .block_on(shutdown.run(&cx, &stopped, lookup(options, &cx)))
        .map_err(tailnet)?;
    Ok(output::hosts(&snapshot, options.json))
}
/// `--socket` names one root tailscaled socket on Linux and macOS; otherwise
/// the installed Tailscale (on macOS the app, or the open-source daemon's).
async fn lookup(options: &Options, cx: &Cx) -> Result<Discovery, TailnetError> {
    let api = match &options.socket {
        Some(path) => fr_tailnet::LocalApi::new(path)?,
        None => fr_tailnet::LocalApi::installed(),
    };
    api.discover(cx).await
}
fn failure(code: &'static str, next: &'static str) -> Failure {
    Failure::new(code, next, 1)
}
pub(super) fn tailnet(error: TailnetError) -> Failure {
    match error {
        TailnetError::Cancelled => Failure::new(
            "cancelled",
            "The lookup/session was stopped; no actions were replayed.",
            130,
        ),
        TailnetError::LocalApiUnavailable => failure(
            "tailscale_unavailable",
            if cfg!(target_os = "macos") {
                "Start the installed Tailscale app, or tailscaled for /var/run/tailscaled.socket or your --socket path."
            } else {
                "Start installed Tailscale and check the protected LocalAPI socket."
            },
        ),
        TailnetError::BackendNotRunning => failure(
            "tailscale_not_running",
            "Connect the installed Tailscale client before retrying.",
        ),
        TailnetError::UntrustedLocalApi => failure(
            "untrusted_localapi",
            if cfg!(target_os = "macos") {
                "Use the protected installed Tailscale app or the root tailscaled socket; do not substitute a user-owned socket or proxy."
            } else {
                "Use the root-owned installed tailscaled endpoint; do not substitute a user-owned proxy."
            },
        ),
        TailnetError::Timeout => failure(
            "tailnet_lookup_timeout",
            "Check the installed Tailscale daemon before retrying.",
        ),
        TailnetError::HandshakeTimeout => failure(
            "host_not_answering",
            "The host did not complete the connection in time: check that frd run is running there (it serves one viewer at a time, so another session may be active) and that tailnet policy allows UDP to its port; fr hosts shows the path.",
        ),
        TailnetError::HostBusy => failure(
            "host_busy",
            "The host is serving another viewer; it serves one at a time. Retry after that session ends.",
        ),
        TailnetError::NativeRouteIo => failure(
            "tailnet_route_unavailable",
            "The local UDP path failed during connection setup. Check the installed Tailscale connection and routing; fr hosts shows the host path.",
        ),
        TailnetError::KeyExpired => failure(
            "tailnet_key_expired",
            "Reauthenticate the affected Tailscale node locally.",
        ),
        TailnetError::PeerCertificateRejected => failure(
            "tls_certificate_rejected",
            "The host's certificate did not verify for the selected tailnet name against the trusted roots (wrong host, expired, or an untrusted issuer; the TLS stack does not say which). Check the host's Tailscale certificate and any --trust-roots; never disable certificate checks.",
        ),
        TailnetError::NativeHandshake
        | TailnetError::CertificateRejected
        | TailnetError::InvalidTrustStore => failure(
            "tls_identity_refused",
            "Verify the locally provisioned CA roots and selected canonical host; never disable certificate checks.",
        ),
        _ => failure(
            "tailnet_identity_refused",
            "Inspect installed Tailscale identity, sharing and policy; rerun fr hosts before reconnecting.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_authority_reads_never_report_an_empty_successful_snapshot() {
        for (error, code, exit) in [
            (
                TailnetError::LocalApiUnavailable,
                "tailscale_unavailable",
                1,
            ),
            (TailnetError::UntrustedLocalApi, "untrusted_localapi", 1),
            (TailnetError::BackendNotRunning, "tailscale_not_running", 1),
            (TailnetError::Timeout, "tailnet_lookup_timeout", 1),
            (TailnetError::HostBusy, "host_busy", 1),
            (TailnetError::Cancelled, "cancelled", 130),
        ] {
            let refusal = tailnet(error);
            assert_eq!(refusal.exit, exit);
            let json: serde_json::Value =
                serde_json::from_str(&output::failure(refusal, true)).unwrap();
            assert_eq!(json["error"]["code"], code);
            assert_ne!(json["outcome"], "success");
            assert!(json.get("peers").is_none());
        }
    }
}
