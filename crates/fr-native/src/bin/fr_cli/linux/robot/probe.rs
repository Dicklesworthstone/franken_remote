//! Explicit native capability inspection on the canonical authenticated dialer.
//! Only an acknowledged pre-observation close produces a successful report.
use super::{
    Cell, Cx, Failure, InspectOptions, LocalApi, RobotInspectData, Runtime, Shutdown, failure,
    now_unix_ms, tailnet,
};
use crate::options::InspectProbe;
use fr_client::robot::{
    AcknowledgementStage, RobotEnvelope, inspect::capabilities::CapabilityInspectionData,
};
use fr_tailnet::{DiscoveredPeer, PeerSelector};
use frd::{
    native_connection::{self, AddressFamily, Client, Configuration},
    session_startup,
};
use std::time::Duration;

const PROBE_BUDGET: Duration = Duration::from_secs(5);
const STARTUP_BUDGET: Duration = Duration::from_secs(2);

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    api: &LocalApi,
    peer: &DiscoveredPeer,
    options: &InspectOptions,
    probe: &InspectProbe,
    identity: RobotInspectData,
    json: bool,
) -> Result<String, Failure> {
    let roots = super::super::read_roots(&probe.roots)?;
    let mut client =
        Client::new(api.clone(), roots, Duration::from_secs(3)).map_err(connection_error)?;
    // Recheck BOTH values during each fresh LocalAPI resolution and after TLS.
    // The discovery snapshot alone must never authorize a replacement machine.
    let selector = PeerSelector::Pinned {
        stable_id: peer.stable_id(),
        certificate_name: peer.certificate_name(),
    };
    let configuration = Configuration {
        port: options.port,
        family: if probe.ipv6 {
            AddressFamily::Ipv6
        } else {
            AddressFamily::Automatic
        },
        startup_timeout: STARTUP_BUDGET,
        ..Default::default()
    };
    let offer = fr_client::native::inspection_offer().map_err(|_| {
        failure(
            "capability_inspection_configuration",
            "This build cannot construct a bounded native capability inspection offer.",
        )
    })?;
    let operation = client.run(cx.clone(), selector, configuration, offer, |viewer| {
        viewer.inspect_capabilities()
    });
    let bounded = asupersync::time::timeout(cx.now(), PROBE_BUDGET, operation);
    let result = runtime.block_on(shutdown.run(cx, stopped, bounded));
    if stopped.get() {
        return Err(Failure::new(
            "cancelled",
            "Capability inspection cancelled before opening observation.",
            130,
        ));
    }
    let capabilities = result
        .map_err(|_| inspection_timeout())?
        .map_err(connection_error)?
        .map_err(inspection_error)?;
    let report = CapabilityInspectionData::after_acknowledged_probe(identity, capabilities)
        .map_err(|_| {
            failure(
                "capability_inspection_invalid",
                "The host did not provide a valid pre-observation capability intersection.",
            )
        })?;
    let human = (!json).then(|| report.render_human());
    let envelope = RobotEnvelope::success(now_unix_ms(), AcknowledgementStage::Observed, report);
    if json {
        envelope.render_json().map_err(|_| {
            failure(
                "serialization_error",
                "Failed to render capability inspection.",
            )
        })
    } else {
        Ok(envelope.render_human(human.as_deref()))
    }
}

fn inspection_timeout() -> Failure {
    failure(
        "capability_inspection_timeout",
        "The original capability-inspection budget expired without an acknowledged close. Check host availability; no observation was opened. A new inspection requires an explicit request.",
    )
}

fn connection_error(error: native_connection::Error) -> Failure {
    if let Some(refused) = super::super::refusal::connection(error) {
        return refused;
    }
    match error {
        native_connection::Error::Tailnet(error) => tailnet(error),
        native_connection::Error::Session(error) => inspection_error(error),
        native_connection::Error::Cancelled => Failure::new(
            "cancelled",
            "Capability inspection cancelled before opening observation.",
            130,
        ),
        _ => failure(
            "capability_inspection_connection_failed",
            "Verify host identity, native transport and the locally configured trust roots.",
        ),
    }
}

fn inspection_error(error: session_startup::Error) -> Failure {
    if let Some(refused) = super::super::refusal::session(error) {
        return refused;
    }
    match error {
        session_startup::Error::Expired
        | session_startup::Error::ClientStartup(fr_client::startup::Error::Expired) => {
            inspection_timeout()
        }
        session_startup::Error::Cancelled => Failure::new(
            "cancelled",
            "Capability inspection cancelled before opening observation.",
            130,
        ),
        _ => failure(
            "capability_inspection_refused",
            "The host did not complete the pre-observation inspection exchange. Use compatible builds; no fallback session was opened.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_wire::{
        negotiation,
        refusal::{Reason, Refused},
    };

    #[test]
    fn unsupported_probe_keeps_the_hosts_typed_refusal() {
        let error = session_startup::Error::Protocol(negotiation::Error::Refused(Refused {
            reason: Reason::RequiredCapability,
            operation: None,
            stage: None,
        }));
        assert_eq!(
            inspection_error(error).code,
            "host_required_capability_missing"
        );
        assert_eq!(
            connection_error(native_connection::Error::Session(error)).code,
            "host_required_capability_missing"
        );
    }

    #[test]
    fn expiration_cancellation_and_identity_changes_remain_distinct() {
        assert_eq!(
            inspection_error(session_startup::Error::Expired).code,
            "capability_inspection_timeout"
        );
        let expired = session_startup::Error::ClientStartup(fr_client::startup::Error::Expired);
        assert_eq!(
            inspection_error(expired).code,
            "capability_inspection_timeout"
        );
        assert_eq!(
            connection_error(native_connection::Error::Session(expired)).code,
            "capability_inspection_timeout"
        );
        let cancelled = inspection_error(session_startup::Error::Cancelled);
        assert_eq!(cancelled.code, "cancelled");
        assert_eq!(cancelled.exit, 130);
        let identity = connection_error(native_connection::Error::Tailnet(
            fr_tailnet::Error::IdentityChanged,
        ));
        assert_ne!(identity.code, "capability_inspection_timeout");
        assert_ne!(identity.code, "cancelled");
    }
}
