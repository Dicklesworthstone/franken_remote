//! Periodic, release-only snapshots from the local platform's actual held set.
use super::{ClientInstant, Error, InputClient, StopReason};
use fr_core::{
    held_state::{HeldState, HeldStateRequest},
    input::{PhysicalKey, PointerButton},
};
use fr_wire::{
    held_state::encode,
    input::{InputDelivery, InputDirection},
};

/// At most four snapshots per second. Key/button transitions remain immediate.
pub const HELD_STATE_INTERVAL_US: u64 = 250_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct EncodedHeldState {
    pub bytes: usize,
    pub sequence: u64,
    pub next_action: u64,
}
impl InputClient {
    /// Sample actual local platform state, not this client's remembered presses.
    /// Only keys/buttons previously sent by this owner can remain held. Missing
    /// state clears locally and requests host releases; extras never create a
    /// press. No action identity, receipt slot, ticket or view is renewed.
    ///
    /// Send the exact bytes on the SAME ordered stream after preceding actions
    /// and before later actions, or stop this owner. A pending snapshot must not
    /// be moved past a newer press. None means the bounded cadence is not due.
    /// A stopped/hidden session uses independent host revocation, not this API.
    pub fn reconcile_held(
        &mut self,
        observed: HeldState,
        out: &mut [u8],
        now: ClientInstant,
    ) -> Result<Option<EncodedHeldState>, Error> {
        self.tick(now)?;
        if self.held_after.is_some_and(|at| now < at) {
            return Ok(None);
        }
        let (Some(sequence), Some(next_action)) = (self.next_held, self.next_action) else {
            return self.fail(StopReason::CounterExhausted);
        };
        let Some(next_due) = now.0.checked_add(HELD_STATE_INTERVAL_US) else {
            return self.fail(StopReason::CounterExhausted);
        };
        let mut held = HeldState::empty();
        for usage in 0_u16..256 {
            if let Some(key) = PhysicalKey::new(usage)
                && self.keys[usize::from(usage)]
                && observed.key(key)
            {
                held.set_key(key, true);
            }
        }
        for button in [
            PointerButton::Primary,
            PointerButton::Secondary,
            PointerButton::Middle,
            PointerButton::Back,
            PointerButton::Forward,
        ] {
            held.set_button(
                button,
                self.buttons[button as usize - 1] && observed.button(button),
            );
        }
        let bytes = encode(
            HeldStateRequest {
                session: self.credentials.session,
                lease: self.credentials.lease,
                sequence,
                next_action,
                held,
            },
            out,
            &self.limits,
            self.binding.channel,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        // Failed encoding has not changed any sequence or remembered held state.
        for usage in 0_u16..256 {
            if let Some(key) = PhysicalKey::new(usage) {
                self.keys[usize::from(usage)] &= held.key(key);
            }
        }
        for (index, remembered) in self.buttons.iter_mut().enumerate() {
            *remembered &= held.button_bits() & (1 << index) != 0;
        }
        self.next_held = sequence.checked_add(1);
        self.held_after = Some(ClientInstant(next_due));
        Ok(Some(EncodedHeldState {
            bytes,
            sequence,
            next_action,
        }))
    }
}
