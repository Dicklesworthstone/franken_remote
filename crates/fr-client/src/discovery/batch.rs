//! A one-shot capability sweep. Own every in-flight probe until completion or
//! cancellation; a failed or slow peer cannot serialize the other three slots.
use std::{
    future::{Future, poll_fn},
    pin::Pin,
    task::Poll,
};

/// Per-command candidate and concurrency bounds, independent of tailnet size.
pub const MAX_BATCH_TARGETS: usize = 64;
pub const MAX_BATCH_CONCURRENCY: usize = 4;

#[derive(Debug, PartialEq, Eq)]
pub enum ProbeOutcome<T> {
    /// No probe future was constructed for this candidate.
    NotStarted,
    /// The original probe was dropped when the sweep's permission ended.
    Interrupted,
    /// The original future completed; this may itself contain a typed refusal.
    Completed(T),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooManyTargets;

type Slot<F> = Option<(usize, Pin<Box<F>>)>;
fn interrupted<F: Future>(
    active: &mut [Slot<F>],
    outcomes: &mut Vec<ProbeOutcome<F::Output>>,
) -> Vec<ProbeOutcome<F::Output>> {
    for slot in active {
        if let Some((index, future)) = slot.take() {
            drop(future);
            outcomes[index] = ProbeOutcome::Interrupted;
        }
    }
    std::mem::take(outcomes)
}

/// Drive a bounded set of independently owned probe futures, returning results
/// in candidate order. `permitted` MUST include the original whole-sweep
/// deadline and cancellation; its owner must arrange a wake at that deadline.
/// Each probe additionally owns its own timeout and cancellation discipline.
///
/// No retry, background task, detached future, or identity cache is introduced.
/// At most four new futures are constructed in one poll, even if every peer
/// completes immediately. This leaves the supervisor time to service shutdown.
pub fn probe_batch<F: Future>(
    targets: usize,
    mut start: impl FnMut(usize) -> F,
    mut permitted: impl FnMut() -> bool,
) -> Result<impl Future<Output = Vec<ProbeOutcome<F::Output>>>, TooManyTargets> {
    if targets > MAX_BATCH_TARGETS {
        return Err(TooManyTargets);
    }
    let mut outcomes: Vec<_> = (0..targets).map(|_| ProbeOutcome::NotStarted).collect();
    let mut active: [Slot<F>; MAX_BATCH_CONCURRENCY] = std::array::from_fn(|_| None);
    let (mut next, mut completed) = (0, 0);
    Ok(poll_fn(move |task| {
        for slot_index in 0..MAX_BATCH_CONCURRENCY {
            // Check before BOTH launching and polling a probe. A preceding
            // completion may have ended the parent or consumed its budget.
            if !permitted() {
                return Poll::Ready(interrupted(&mut active, &mut outcomes));
            }
            let slot = &mut active[slot_index];
            if slot.is_none() && next < targets {
                *slot = Some((next, Box::pin(start(next))));
                next += 1;
            }
            if !permitted() {
                return Poll::Ready(interrupted(&mut active, &mut outcomes));
            }
            let slot = &mut active[slot_index];
            if let Some((index, future)) = slot
                && let Poll::Ready(result) = future.as_mut().poll(task)
            {
                let index = *index;
                if !permitted() {
                    return Poll::Ready(interrupted(&mut active, &mut outcomes));
                }
                outcomes[index] = ProbeOutcome::Completed(result);
                active[slot_index] = None;
                completed += 1;
            }
        }
        if completed == targets {
            Poll::Ready(std::mem::take(&mut outcomes))
        } else {
            // A vacated slot can start the next peer on the next turn. Pending
            // futures keep their own wakers; never busy-poll a full flight.
            if next < targets && active.iter().any(Option::is_none) {
                task.waker().wake_by_ref();
            }
            Poll::Pending
        }
    }))
}

#[cfg(test)]
mod tests;
