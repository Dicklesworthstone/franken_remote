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
    /// When the exact union exceeds the negotiated range count, bridge the
    /// smallest gaps instead of refusing new losses. Every requested fragment
    /// remains represented; the ordinary byte, round and deadline gates still
    /// apply to the extra retransmissions. Storage and work remain bounded.
    pub(super) fn coalesce(
        &self,
        requested: &[RepairRange],
        maximum: usize,
    ) -> Result<Self, SendError> {
        let maximum = maximum.min(self.ranges.len());
        if maximum == 0 {
            return Err(SendError::RepairBusy);
        }
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
            if result.count < maximum {
                result.ranges[result.count] = next;
                result.count += 1;
                continue;
            }
            // Input runs arrive in increasing start order. Retain the largest
            // maximum-1 gaps in that union: all skipped interior fragments have
            // the same wire cost, so this minimizes duplicate repair bytes.
            // Ties bridge the newest gap, keeping the result deterministic.
            let mut merge = result.count - 1;
            let mut gap = next.start - result.ranges[merge].end;
            for i in 0..result.count - 1 {
                let candidate = result.ranges[i + 1].start - result.ranges[i].end;
                if candidate < gap {
                    merge = i;
                    gap = candidate;
                }
            }
            if merge == result.count - 1 {
                result.ranges[merge].end = next.end;
            } else {
                result.ranges[merge].end = result.ranges[merge + 1].end;
                result
                    .ranges
                    .copy_within(merge + 2..result.count, merge + 1);
                result.ranges[result.count - 1] = next;
            }
        }
        // Both lists are ordered/disjoint before union. Use a wide count so a
        // peer's ranges cannot overflow even at the maximum fragment index.
        // A subset of the pending suffix never requires compaction (that suffix
        // already fits maximum), so bridging cannot turn a duplicate into work.
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
