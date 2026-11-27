//! Convert one verified browser negotiation into the actual capture owner.
use super::{Gate, context};
use crate::{
    browser_ingress::desktop::{Authorized, Factory, Request},
    host_run::{Encoder, Options, capture_launch, choose, random_nonzero_u128},
    media::{ObservationControl, host_now},
};
use asupersync::runtime::RuntimeHandle;
use fr_core::authority::{AuthorityPolicy, SessionAuthority};
use fr_wire::{
    negotiation::{Role, browser},
    refusal::Reason,
};
use std::sync::Arc;

pub(super) fn make(
    options: &Options,
    encoder: Encoder,
    runtime: &RuntimeHandle,
    gate: &Gate,
) -> Factory {
    let (worker, display, xauthority) = (
        options.worker.clone(),
        options.display.clone(),
        options.xauthority.clone(),
    );
    let (fps, bitrate, once) = (options.fps, options.bitrate, options.once);
    let runtime = runtime.clone();
    let gate = gate.clone();
    Arc::new(move |request: Request| {
        let (worker, display, xauthority) = (worker.clone(), display.clone(), xauthority.clone());
        let runtime = runtime.clone();
        let gate = gate.clone();
        Box::pin(async move {
            gate.check().map_err(|_| Reason::PermissionDenied)?;
            request
                .peer
                .observe()
                .map_err(|_| Reason::TailnetMembershipUnverifiable)?;
            if request.selection.role != Role::Observe {
                return Err(Reason::UnsupportedProfile);
            }
            // The same live policy epoch was selected before socket admission.
            // It requires no local approval; no peer or bootstrap changes it.
            let local = gate.policy.check().map_err(|_| Reason::PermissionDenied)?;
            if local.approval_mode != crate::host_policy::Approval::None {
                return Err(Reason::LocalApprovalDenied);
            }
            let mut authority = SessionAuthority::new(
                request.binding.remote_session,
                AuthorityPolicy::plan_defaults(),
            );
            authority
                .mark_capabilities_checked()
                .map_err(|_| Reason::InvalidState)?;
            authority
                .authorize_observation(host_now(&request.cx).map_err(|_| Reason::HostUnavailable)?)
                .map_err(|_| Reason::PermissionDenied)?;
            let control = ObservationControl::new_admitted(request.cx, authority, request.peer)
                .map_err(|_| Reason::PermissionDenied)?;
            let launch = capture_launch(
                &worker,
                &display,
                xauthority.as_deref(),
                random_nonzero_u128().map_err(|_| Reason::HostUnavailable)?,
            )
            .map_err(|_| Reason::HostUnavailable)?;
            let maximum = request
                .selection
                .limits
                .max_encoded_access_unit_bytes()
                .min(browser::MAX_ACCESS_UNIT_BYTES);
            let cleanup = context(&runtime).map_err(|_| Reason::HostUnavailable)?;
            let finish = StopAfterShare {
                once,
                stop: gate.stop.clone(),
            };
            Ok(Authorized {
                control,
                launch,
                cleanup,
                select: Box::new(move |catalog| {
                    let (selected, mut codec) = choose(catalog, fps, bitrate, encoder)?;
                    codec.max_access_unit_bytes = maximum;
                    Ok((selected, codec))
                }),
                supervise: Box::pin(async move {
                    let _finish = finish;
                    gate.watch().await
                }),
            })
        })
    })
}

/// The original accepted desktop, including failed startup, consumes --once.
/// Closing an ordinary asset or bootstrap HTTP request never reaches this guard.
struct StopAfterShare {
    once: bool,
    stop: Arc<super::StopHandle>,
}
impl Drop for StopAfterShare {
    fn drop(&mut self) {
        if self.once {
            self.stop.request();
        }
    }
}
