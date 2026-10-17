//! The same role-specific ceiling applies before reservation and promotion.
//! This calculation never allocates, changes credit, or grants authority.
use fr_core::limits::ProtocolLimits;
use fr_wire::{attachment::MediaRole, input::MAX_INPUT_RECORD_BYTES};

pub(super) fn maximum(
    limits: &ProtocolLimits,
    role: MediaRole,
    stream_window: u64,
    critical_bytes: usize,
    bulk_bytes: usize,
    datagram_bytes: usize,
) -> u64 {
    // Clipboard and files use the bulk pool in BOTH directions. Their records
    // must not be capped at the unrelated critical reserve. Recovery remains
    // asymmetric: its reverse route is critical, so it still fits both pools.
    let send_bytes = match role {
        MediaRole::Clipboard | MediaRole::Files => bulk_bytes,
        MediaRole::Recovery => bulk_bytes.min(critical_bytes),
        _ => critical_bytes,
    };
    let base = u64::from(limits.max_control_message_bytes())
        .min(stream_window)
        .min(send_bytes as u64);
    match role {
        MediaRole::Configuration
        | MediaRole::Recovery
        | MediaRole::Clipboard
        | MediaRole::Files => base,
        // One cap covers reliable replies/progress and packet datagrams.
        MediaRole::Video | MediaRole::AudioDown => base.min(datagram_bytes as u64),
        MediaRole::Input => base.min(MAX_INPUT_RECORD_BYTES as u64),
    }
}

/// Capability absence uses the exact legacy role calculation. Do not infer
/// peer support from a large generic protocol cap: older peers advertise that
/// cap too, but reject larger bulk grants during attachment.
pub(super) fn negotiated_maximum(
    selection: &fr_wire::negotiation::Selection,
    role: MediaRole,
    stream_window: u64,
    critical_bytes: usize,
    bulk_bytes: usize,
    datagram_bytes: usize,
) -> u64 {
    let bulk_bytes = if fr_wire::attachment::bulk_records_selected(selection) {
        bulk_bytes
    } else {
        bulk_bytes.min(critical_bytes)
    };
    maximum(
        &selection.limits,
        role,
        stream_window,
        critical_bytes,
        bulk_bytes,
        datagram_bytes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::limits::LimitOverrides;

    #[test]
    fn bulk_uses_its_own_pool_and_retains_every_other_role_bound() {
        let limits = ProtocolLimits::ABSOLUTE;
        for critical in [1024, 4096, 8192, 16_384] {
            for role in [MediaRole::Clipboard, MediaRole::Files] {
                assert_eq!(
                    maximum(&limits, role, 65_536, critical, 131_072, 1150),
                    65_536
                );
            }
            for role in [MediaRole::Configuration, MediaRole::Recovery] {
                assert_eq!(
                    maximum(&limits, role, 65_536, critical, 131_072, 1150),
                    critical as u64
                );
            }
            for role in [MediaRole::Video, MediaRole::AudioDown] {
                assert_eq!(
                    maximum(&limits, role, 65_536, critical, 131_072, 1150),
                    critical.min(1150) as u64
                );
            }
            assert_eq!(
                maximum(&limits, MediaRole::Input, 65_536, critical, 131_072, 1150),
                critical.min(MAX_INPUT_RECORD_BYTES) as u64
            );
        }
    }

    #[test]
    fn each_negotiated_or_storage_bound_can_be_the_limiting_factor() {
        for (record, stream, bulk, expected) in [
            (20_000, 65_536, 131_072, 20_000),
            (65_536, 24_000, 131_072, 24_000),
            (65_536, 65_536, 32_000, 32_000),
            (65_536, 65_536, 4096, 4096),
        ] {
            let limits = ProtocolLimits::with_overrides(LimitOverrides {
                max_control_message_bytes: Some(record),
                ..Default::default()
            })
            .unwrap();
            for role in [MediaRole::Clipboard, MediaRole::Files] {
                assert_eq!(maximum(&limits, role, stream, 8192, bulk, 1150), expected);
            }
        }
    }

    #[test]
    fn recovery_still_fits_both_pools_and_datagrams_keep_their_ceiling() {
        let limits = ProtocolLimits::ABSOLUTE;
        assert_eq!(
            maximum(&limits, MediaRole::Recovery, 65_536, 8192, 1024, 1150),
            1024
        );
        for role in [MediaRole::Video, MediaRole::AudioDown] {
            assert_eq!(maximum(&limits, role, 65_536, 8192, 131_072, 512), 512);
            assert_eq!(maximum(&limits, role, 256, 8192, 131_072, 1150), 256);
        }
    }

    #[test]
    fn zero_and_extreme_budgets_never_wrap_or_escape_the_selected_limits() {
        let limits = ProtocolLimits::ABSOLUTE;
        for role in [
            MediaRole::Configuration,
            MediaRole::Recovery,
            MediaRole::Video,
            MediaRole::Input,
            MediaRole::Clipboard,
            MediaRole::Files,
            MediaRole::AudioDown,
        ] {
            for stream in [0, 1, 64, 8192, 65_536, u64::MAX] {
                for budget in [0, 1, 64, 8192, 131_072, usize::MAX] {
                    let n = maximum(&limits, role, stream, budget, budget, budget);
                    assert!(n <= stream);
                    assert!(n <= budget as u64);
                    assert!(n <= u64::from(limits.max_control_message_bytes()));
                    if stream == 0 || budget == 0 {
                        assert_eq!(n, 0);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod negotiation_tests {
    use super::*;
    use fr_wire::{
        attachment,
        negotiation::{Capability, Offer, Role},
    };
    pub(super) fn offer(bulk: bool) -> Offer {
        Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role: Role::RequestControl,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities: if bulk {
                vec![Capability {
                    name: attachment::BULK_RECORD_CAPABILITY.into(),
                    version: attachment::BULK_RECORD_VERSION,
                    required: false,
                }]
            } else {
                vec![]
            },
        }
    }
    #[test]
    fn both_peers_must_select_the_larger_bulk_ceiling() {
        for host in [false, true] {
            for client in [false, true] {
                let selection = offer(host)
                    .intersect(&offer(client))
                    .unwrap()
                    .select()
                    .unwrap();
                for role in [MediaRole::Clipboard, MediaRole::Files] {
                    assert_eq!(
                        negotiated_maximum(&selection, role, 65_536, 8192, 131_072, 1150),
                        if host && client { 65_536 } else { 8192 }
                    );
                }
                for role in [
                    MediaRole::Configuration,
                    MediaRole::Recovery,
                    MediaRole::Video,
                    MediaRole::Input,
                    MediaRole::AudioDown,
                ] {
                    assert_eq!(
                        negotiated_maximum(&selection, role, 65_536, 8192, 131_072, 1150),
                        maximum(&selection.limits, role, 65_536, 8192, 131_072, 1150)
                    );
                }
            }
        }
    }
    #[test]
    fn wrong_role_or_unknown_version_never_raises_legacy_allowance() {
        let selected = offer(true).select().unwrap();
        let mut observe = selected.clone();
        observe.role = Role::Observe;
        let mut future = selected;
        future.capabilities[0].version += 1;
        for selection in [observe, future] {
            assert!(!attachment::bulk_records_selected(&selection));
            for role in [MediaRole::Clipboard, MediaRole::Files] {
                assert_eq!(
                    negotiated_maximum(&selection, role, 65_536, 8192, 131_072, 1150),
                    8192
                );
            }
        }
    }
    #[test]
    fn positive_selection_does_not_override_smaller_peer_limits_or_local_storage() {
        let host = offer(true);
        let mut client = host.clone();
        client.limits = ProtocolLimits::with_overrides(fr_core::limits::LimitOverrides {
            max_control_message_bytes: Some(6000),
            ..Default::default()
        })
        .unwrap();
        let selection = host.intersect(&client).unwrap().select().unwrap();
        assert!(attachment::bulk_records_selected(&selection));
        for role in [MediaRole::Clipboard, MediaRole::Files] {
            assert_eq!(
                negotiated_maximum(&selection, role, 65_536, 8192, 131_072, 1150),
                6000
            );
            assert_eq!(
                negotiated_maximum(&selection, role, 65_536, 8192, 4096, 1150),
                4096
            );
        }
    }
}

#[cfg(test)]
#[path = "record_limit/clipboard.rs"]
mod clipboard;
