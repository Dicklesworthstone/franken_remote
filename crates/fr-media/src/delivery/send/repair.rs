//! Bounded union of a live repair's unsent suffix and a new validated request.
//! This operates only on range metadata: no payload copy, queue or allocation.
use super::{RepairJob, SendError};
use fr_wire::RepairRange;

impl RepairJob {
    fn remaining(&self) -> impl Iterator<Item = RepairRange> + '_ {
        self.ranges[self.range..self.count]
            .iter()
            .copied()
            .enumerate()
            .map(|(index, mut range)| {
                if index == 0 {
                    range.start = self.fragment;
                }
                range
            })
    }

    /// Keep all outstanding fragments, adding only genuinely new work. A new
    /// request may explicitly request an already emitted (and lost) repair;
    /// merely normalizing adjacent ranges must not consume another round.
    pub(super) fn coalesce(
        &self,
        requested: &[RepairRange],
        maximum: usize,
    ) -> Result<Self, SendError> {
        let mut old = self.remaining().peekable();
        let mut new = requested.iter().copied().peekable();
        let mut result = Self {
            ranges: [RepairRange { start: 0, end: 0 }; 64],
            count: 0,
            range: 0,
            fragment: 0,
        };
        while let Some(next) = match (old.peek(), new.peek()) {
            (Some(a), Some(b)) if a.start <= b.start => old.next(),
            (_, Some(_)) => new.next(),
            (Some(_), None) => old.next(),
            (None, None) => None,
        } {
            if result.count != 0 {
                let last = &mut result.ranges[result.count - 1];
                if next.start <= last.end {
                    last.end = last.end.max(next.end);
                    continue;
                }
            }
            if result.count >= maximum.min(result.ranges.len()) {
                return Err(SendError::RepairBusy);
            }
            result.ranges[result.count] = next;
            result.count += 1;
        }
        // Both lists are ordered/disjoint before union. Use a wide count so a
        // peer's ranges cannot overflow even at the maximum fragment index.
        let old_count: u64 = self.remaining().map(|r| u64::from(r.end - r.start)).sum();
        let new_count: u64 = result.ranges[..result.count]
            .iter()
            .map(|r| u64::from(r.end - r.start))
            .sum();
        if old_count == new_count {
            return Err(SendError::RepairBusy);
        }
        result.fragment = result.ranges[0].start;
        Ok(result)
    }
}

#[cfg(test)]
#[path = "repair/tests.rs"]
mod tests;
