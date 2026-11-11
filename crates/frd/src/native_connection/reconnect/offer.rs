//! Per-attempt removal of optional advertisements, never authority or replay.
use super::{Application, Failure, Offer};

pub(super) fn prepare(
    application: &mut impl Application,
    attempt: u8,
    original: &Offer,
) -> Result<Offer, Failure> {
    let selected = application
        .offer(attempt, original)
        .map_err(|_| Failure::Notification)?;
    // validate bounds the lists and rejects duplicated/reordered capability
    // names before the subset comparisons. The original was validated before
    // entering the reconnect loop and remains immutable across all attempts.
    if selected.validate().is_err()
        || selected.versions != original.versions
        || selected.profile != original.profile
        || selected.profile_version != original.profile_version
        || selected.role != original.role
        || selected.limits != original.limits
        || selected
            .capabilities
            .iter()
            .any(|capability| !original.capabilities.contains(capability))
        || original
            .capabilities
            .iter()
            .any(|capability| capability.required && !selected.capabilities.contains(capability))
    {
        return Err(Failure::Notification);
    }
    Ok(selected)
}

/// This attempt's stated picture box. One the host could not accept (an axis
/// below the minimum) keeps native pictures instead of failing the attempt.
pub(super) fn picture(
    application: &mut impl Application,
    attempt: u8,
    original: Option<(u32, u32)>,
) -> Option<(u32, u32)> {
    application
        .picture(attempt, original)
        .filter(|(width, height)| {
            (*width).min(*height) >= fr_wire::display::picture::MIN_BOX_PIXELS
        })
}
