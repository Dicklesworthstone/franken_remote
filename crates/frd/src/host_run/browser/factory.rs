//! Convert one verified browser negotiation into the actual capture owner.
use super::{Gate, context};
use crate::{
    browser_ingress::desktop::{Authorized, Factory, Request},
    host_run::{Encoder, Options, approval, capture_launch, choose, random_nonzero_u128},
    media::{ObservationControl, host_now},
};
use asupersync::{runtime::RuntimeHandle, time::sleep};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::CodecConfigurationGeneration,
    limits::ProtocolLimits,
};
use fr_media::worker::Configuration;
use fr_wire::{
    display::{Catalog, Select},
    negotiation::{Role, browser},
    refusal::Reason,
};
use std::{sync::Arc, time::Duration};

pub(super) fn make(
    options: &Options,
    encoder: Encoder,
    runtime: &RuntimeHandle,
    gate: &Gate,
    consent: approval::Notify,
) -> Factory {
    let (worker, display, xauthority) = (
        options.worker.clone(),
        options.display.clone(),
        options.xauthority.clone(),
    );
    let (fps, bitrate, once) = (options.fps, options.bitrate, options.once);
    let runtime = runtime.clone();
    let gate = gate.clone();
    Arc::new(move |mut request: Request| {
        let (worker, display, xauthority) = (worker.clone(), display.clone(), xauthority.clone());
        let runtime = runtime.clone();
        let gate = gate.clone();
        let consent = consent.clone();
        Box::pin(async move {
            let authority = approved_authority(&mut request, &gate, &consent).await?;
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
                select: Box::new(move |catalog, requested| {
                    let (selected, mut codec) =
                        choose_source(catalog, requested, fps, bitrate, encoder)?;
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

fn choose_source(
    catalog: &Catalog,
    requested: Option<Select>,
    fps: u16,
    bitrate: u32,
    encoder: Encoder,
) -> Result<(Select, Configuration), ()> {
    let Some(requested) = requested else {
        return choose(catalog, fps, bitrate, encoder);
    };
    let display = catalog.selected(requested).map_err(|_| ())?;
    Ok((
        requested,
        Configuration {
            width: display.pixel_width,
            height: display.pixel_height,
            fps,
            backend: encoder.backend(),
            bitrate,
            max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
            generation: CodecConfigurationGeneration::INITIAL,
        },
    ))
}

async fn approved_authority(
    request: &mut Request,
    gate: &Gate,
    consent: &approval::Notify,
) -> Result<SessionAuthority, Reason> {
    gate.check().map_err(|_| Reason::PermissionDenied)?;
    request
        .peer
        .observe()
        .map_err(|_| Reason::TailnetMembershipUnverifiable)?;
    if request.selection.role != Role::Observe {
        return Err(Reason::UnsupportedProfile);
    }
    // The same live policy epoch was selected before socket admission.
    // Neither a peer nor the bootstrap response can weaken it.
    let local = gate.policy.check().map_err(|_| Reason::PermissionDenied)?;
    let mut authority = SessionAuthority::new(
        request.binding.remote_session,
        AuthorityPolicy::plan_defaults(),
    );
    authority
        .mark_capabilities_checked()
        .map_err(|_| Reason::InvalidState)?;
    if local.approval_mode == crate::host_policy::Approval::Local {
        authority
            .require_approval()
            .map_err(|_| Reason::InvalidState)?;
        let mut pending = request.approval.take().ok_or(Reason::LocalApprovalDenied)?;
        let original = pending.capability().map_err(approval_refusal)?;
        consent
            .submit(&original, Role::Observe)
            .map_err(|_| Reason::LocalApprovalDenied)?;
        loop {
            gate.check().map_err(|_| Reason::PermissionDenied)?;
            request
                .peer
                .observe()
                .map_err(|_| Reason::TailnetMembershipUnverifiable)?;
            if pending.consume().map_err(approval_refusal)? {
                break;
            }
            sleep(request.cx.now(), Duration::from_millis(5)).await;
        }
    } else if request.approval.is_some() {
        return Err(Reason::InvalidState);
    }
    // Native prompt cleanup and consuming its answer took time. Recheck
    // the original run, policy and admitted peer before any observation.
    gate.check().map_err(|_| Reason::PermissionDenied)?;
    request
        .peer
        .observe()
        .map_err(|_| Reason::TailnetMembershipUnverifiable)?;
    authority
        .authorize_observation(host_now(&request.cx).map_err(|_| Reason::HostUnavailable)?)
        .map_err(|_| Reason::PermissionDenied)?;
    Ok(authority)
}

fn approval_refusal(error: crate::session_startup::Error) -> Reason {
    match error {
        crate::session_startup::Error::Expired => Reason::ApprovalExpired,
        _ => Reason::LocalApprovalDenied,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::ids::DisplayGeometryGeneration;
    use fr_wire::display::Display;

    fn catalog() -> Catalog {
        let origin = Display {
            handle: 51,
            geometry: DisplayGeometryGeneration::INITIAL,
            x: 0,
            y: 0,
            pixel_width: 1920,
            pixel_height: 1080,
            logical_width: 1920,
            logical_height: 1080,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
        };
        Catalog::new(
            9,
            &[
                origin,
                Display {
                    handle: 52,
                    x: -1280,
                    pixel_width: 1280,
                    pixel_height: 720,
                    logical_width: 1280,
                    logical_height: 720,
                    ..origin
                },
            ],
            &ProtocolLimits::ABSOLUTE,
        )
        .unwrap()
    }

    #[test]
    fn explicit_second_display_preserves_local_encoder_settings_and_its_native_size() {
        let catalog = catalog();
        let requested = catalog.selection(52).unwrap();
        let (choice, config) =
            choose_source(&catalog, Some(requested), 30, 8_000_000, Encoder::Vaapi).unwrap();
        assert_eq!(choice, requested);
        assert_eq!((config.width, config.height), (1280, 720));
        assert_eq!(config.backend, Encoder::Vaapi.backend());
        assert_eq!(config.fps, 30);
        assert_eq!(config.bitrate, 8_000_000);
        assert_eq!(config.generation, CodecConfigurationGeneration::INITIAL);

        let (legacy, config) =
            choose_source(&catalog, None, 30, 8_000_000, Encoder::Vaapi).unwrap();
        assert_eq!(legacy, catalog.selection(51).unwrap());
        assert_eq!((config.width, config.height), (1920, 1080));
    }

    #[test]
    fn configuration_never_normalizes_a_stale_or_cropped_peer_choice() {
        let catalog = catalog();
        let selected = catalog.selection(52).unwrap();
        for request in [
            Select {
                revision: 10,
                ..selected
            },
            Select {
                width: 640,
                ..selected
            },
        ] {
            assert!(
                choose_source(
                    &catalog,
                    Some(request),
                    30,
                    8_000_000,
                    Encoder::SoftwareExplicit,
                )
                .is_err()
            );
        }
    }
}
