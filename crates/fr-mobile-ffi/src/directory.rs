//! One tailnet directory fetch (`fr_directory_fetch`, `PROTOCOL_DIRECTORY.md`):
//! connect to a host the app already knows exactly as a session would (strict
//! `WebPKI` for the name, tailnet-only addresses, the saved pin), ask once
//! for its directory, and close. The host lists itself and the viewer's own
//! untagged machines; nothing here grants, selects a session, or touches
//! capture. The result crosses to Swift as one JSON document.
//!
//! Every listed machine with a dialable `MagicDNS` name carries a pin of the
//! addresses the host listed, so the app can dial it directly (the resolver
//! may answer only one family; frd may serve only the other) while TLS still
//! verifies the certificate for exactly that name. Nothing is ever invented:
//! a machine the app cannot dial is shown without a pin, and an incomplete
//! listing says so.
use crate::session::{end, tailnet_end};
use asupersync::{runtime::RuntimeBuilder, time::timeout, types::Budget};
use fr_wire::{
    directory::{Entry, Listing},
    negotiation,
    refusal::Reason,
};
use frd::{
    native_connection::{Client, Configuration, Error as ConnectionError, HostName, HostPin},
    session_startup::{Error as StartupError, Viewer},
};
use serde_json::{Value, json};
use std::{net::IpAddr, time::Duration};

/// One fetch's whole bound, beyond its own handshake and directory budgets.
const LIMIT: Duration = Duration::from_secs(30);

/// Where to fetch from: a saved host, as the app would open a session to it.
pub struct Request {
    pub host: HostName,
    pub port: u16,
    pub pin: Option<HostPin>,
    /// Dial `pin`'s own addresses (a machine this app learned from a
    /// directory) instead of resolving the name.
    pub direct: bool,
}

/// What one fetch ended with, before it becomes JSON.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Listed(Listing),
    /// The host's typed refusal (or the client's own: a host that answered
    /// without the directory marker is `RequiredCapability`).
    Refused(Reason),
    /// `end::*` and the content-free typed failure.
    Failed(u32, String),
}

/// Fetch on the calling thread (the FFI gives it its own); returns the JSON.
pub fn fetch(request: Request) -> String {
    let via = request.host.as_str().to_owned();
    to_json(&via, &run(request))
}

fn run(request: Request) -> Outcome {
    let Ok(runtime) = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
    else {
        return Outcome::Failed(end::INTERNAL, "MissingRuntime".into());
    };
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let Ok(roots) = crate::roots::bundled() else {
        return Outcome::Failed(end::INTERNAL, "InvalidTrustStore".into());
    };
    let client = match (request.direct, request.pin) {
        (true, Some(pin)) => Client::named_at(pin, roots, Duration::from_secs(5)),
        (true, None) => return Outcome::Failed(end::INTERNAL, "InvalidConfiguration".into()),
        (false, pin) => Client::named(request.host, pin, roots, Duration::from_secs(5)),
    };
    let Ok(mut client) = client else {
        return Outcome::Failed(end::INTERNAL, "InvalidConfiguration".into());
    };
    let Ok(offer) = fr_client::native::directory_offer() else {
        return Outcome::Failed(end::INTERNAL, "InvalidConfiguration".into());
    };
    let cfg = Configuration {
        port: request.port,
        ..Configuration::default()
    };
    let operation = client.run_named(cx.clone(), cfg, offer, Viewer::fetch_directory);
    match runtime.block_on(timeout(cx.now(), LIMIT, operation)) {
        Err(_) => Outcome::Failed(end::UNREACHABLE, "Timeout".into()),
        Ok(result) => outcome(result),
    }
}

/// Classify one fetch's result. Content-free: typed failures only.
pub fn outcome(result: Result<Result<Listing, StartupError>, ConnectionError>) -> Outcome {
    let startup = match result {
        Ok(Ok(listing)) => return Outcome::Listed(listing),
        Ok(Err(error)) | Err(ConnectionError::Session(error)) => error,
        Err(ConnectionError::Tailnet(error)) => {
            return Outcome::Failed(tailnet_end(error), format!("Tailnet({error:?})"));
        }
        Err(ConnectionError::Cancelled) => {
            return Outcome::Failed(end::CANCELLED, "Cancelled".into());
        }
        Err(error) => return Outcome::Failed(end::INTERNAL, format!("{error:?}")),
    };
    match startup {
        StartupError::ClientStartup(fr_client::startup::Error::Protocol(protocol)) => {
            match protocol {
                negotiation::Error::Refused(refused) => Outcome::Refused(refused.reason),
                negotiation::Error::RequiredCapability => {
                    Outcome::Refused(Reason::RequiredCapability)
                }
                other => Outcome::Failed(end::HOST_REFUSED, format!("Protocol({other:?})")),
            }
        }
        other => Outcome::Failed(end::SESSION_FAILED, format!("{other:?}")),
    }
}

/// A stable name for each refusal, for the app's message.
pub const fn reason_name(reason: Reason) -> &'static str {
    match reason {
        Reason::InvalidMessage => "invalid_message",
        Reason::UnsupportedVersion => "unsupported_version",
        Reason::UnsupportedProfile => "unsupported_profile",
        Reason::RequiredCapability => "required_capability",
        Reason::InvalidLimits => "invalid_limits",
        Reason::InvalidSelection => "invalid_selection",
        Reason::PermissionDenied => "permission_denied",
        Reason::LocalApprovalDenied => "local_approval_denied",
        Reason::ApprovalExpired => "approval_expired",
        Reason::ControlUnavailable => "control_unavailable",
        Reason::ResourceLimit => "resource_limit",
        Reason::InvalidState => "invalid_state",
        Reason::Expired => "expired",
        Reason::HostUnavailable => "host_unavailable",
        Reason::TailnetMembershipUnverifiable => "tailnet_membership_unverifiable",
        Reason::DirectoryDisabled => "directory_disabled",
        Reason::DirectoryUnavailable => "directory_unavailable",
    }
}

/// The pin a listed machine is dialed with: its exact `MagicDNS` name and
/// the tailnet addresses the host listed. None when the app cannot name it.
pub fn pin(entry: &Entry) -> Option<HostPin> {
    let name = HostName::parse(&entry.name).ok()?;
    let addresses: Vec<IpAddr> = entry
        .ipv4
        .map(IpAddr::V4)
        .into_iter()
        .chain(entry.ipv6.map(IpAddr::V6))
        .collect();
    HostPin::from_addresses(name, addresses).ok()
}

pub fn to_json(via: &str, outcome: &Outcome) -> String {
    let value = match outcome {
        Outcome::Listed(listing) => json!({
            "status": "ok",
            "via": via,
            "truncated": listing.truncated,
            "machines": listing.entries.iter().map(machine).collect::<Vec<_>>(),
        }),
        Outcome::Refused(reason) => json!({
            "status": "refused",
            "via": via,
            "reason": reason_name(*reason),
        }),
        Outcome::Failed(code, detail) => json!({
            "status": "failed",
            "via": via,
            "end": code,
            "detail": detail,
        }),
    };
    value.to_string()
}

fn machine(entry: &Entry) -> Value {
    json!({
        "name": entry.name,
        "node_id": entry.node_id,
        "os": entry.os,
        "online": entry.online,
        "serving": entry.serving,
        "ipv4": entry.ipv4.map(|ip| ip.to_string()),
        "ipv6": entry.ipv6.map(|ip| ip.to_string()),
        "pin": pin(entry).map(|pin| pin.to_string()),
    })
}

#[cfg(test)]
mod tests;
