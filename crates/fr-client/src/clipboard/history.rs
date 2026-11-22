//! Content-free evidence across a native window's original clipboard owners.
//!
//! A replacement records its retired owner's final snapshot exactly once.
//! Current-owner snapshots are combined without mutating that history, so
//! repeated completion reads cannot duplicate receipts. No clipboard content,
//! native handle, consent or authority survives in this fixed-size value.
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: u8 = 1;

/// A terminal cleanup label can precede collection of its final receipt.
/// Only positive original-owner collection makes a snapshot final.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Collection {
    #[default]
    Unconfirmed,
    Finished,
}

/// Observed native cleanup, independent of configuration and OS publication.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cleanup {
    Unknown,
    #[default]
    NotStarted,
    Pending,
    Finished,
    Failed,
}

impl Cleanup {
    fn retain(self, other: Self) -> Self {
        use Cleanup::{Failed, Finished, NotStarted, Pending, Unknown};
        match (self, other) {
            (Failed, _) | (_, Failed) => Failed,
            (Unknown, _) | (_, Unknown) => Unknown,
            (Pending, _) | (_, Pending) => Pending,
            (Finished, _) | (_, Finished) => Finished,
            (NotStarted, NotStarted) => NotStarted,
        }
    }
}

/// One original owner's observed counters, never permission for a new owner.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub requested: bool,
    pub configured: bool,
    pub native_started: bool,
    pub received_submitted: u64,
    pub received_unknown: u64,
    pub received_refused: u64,
    pub cleanup: Cleanup,
    pub collection: Collection,
}

impl Snapshot {
    fn consistent(self) -> bool {
        (!self.configured || self.requested)
            && (!self.native_started || self.configured)
            && (self.received_submitted == 0 || self.native_started)
            && (self.received_unknown == 0 || self.native_started)
            && (self.received_refused == 0 || self.configured)
            && (!self.native_started || self.cleanup != Cleanup::NotStarted)
    }

    fn saturated(self) -> bool {
        [
            self.received_submitted,
            self.received_unknown,
            self.received_refused,
        ]
        .contains(&u64::MAX)
    }

    /// The original native work and every reported effect have known outcomes.
    /// A known refusal is not an unknown publication and does not imply success.
    pub fn known(self) -> bool {
        self.consistent()
            && self.collection == Collection::Finished
            && !self.saturated()
            && self.received_unknown == 0
            && matches!(self.cleanup, Cleanup::NotStarted | Cleanup::Finished)
    }
}

/// Explicit coverage prevents an earlier clean owner from certifying a later
/// omitted one. Overflow and invalid owner accounting remain terminal evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub schema_version: u8,
    pub owners: u64,
    pub complete: bool,
    pub overflowed: bool,
}

impl Default for Coverage {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            owners: 0,
            complete: true,
            overflowed: false,
        }
    }
}

/// The existing scalar clipboard fields plus bounded original-owner coverage.
/// Platform-specific content-free reason metadata can accompany this document.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completion {
    #[serde(flatten)]
    pub snapshot: Snapshot,
    pub history: Coverage,
}

impl Completion {
    pub fn from_document(document: &serde_json::Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(document)
    }

    /// The caller supplies its independently counted original session owners.
    /// No missing, duplicate, overflowed or unsupported history proves coverage.
    pub fn covers(self, expected_owners: u64) -> bool {
        expected_owners != 0
            && self.history.schema_version == SCHEMA_VERSION
            && self.history.owners == expected_owners
            && self.history.complete
            && !self.history.overflowed
            && self.snapshot.consistent()
            && self.snapshot.collection == Collection::Finished
            && !self.snapshot.saturated()
    }

    pub fn known(self, expected_owners: u64) -> bool {
        self.covers(expected_owners) && self.snapshot.known()
    }
}

/// Fixed-memory cumulative evidence. An owner is its zero-based position in
/// the caller's actual sequence of successful session replacements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct History(Completion);

impl Default for History {
    fn default() -> Self {
        Self(Completion {
            snapshot: Snapshot {
                // Identity for the fold, never proof of an original owner:
                // coverage with zero owners is always refused.
                collection: Collection::Finished,
                ..Snapshot::default()
            },
            ..Completion::default()
        })
    }
}

impl History {
    /// Fold only after replacing the actual original session. An unsuccessful
    /// replacement must leave both this history and the owner's ordinal alone.
    /// Repeated or skipped ordinals invalidate coverage without double-counting.
    pub fn retire(&mut self, owner: u64, snapshot: Snapshot) {
        if owner != self.0.history.owners {
            self.0.history.complete = false;
            return;
        }
        let Some(owners) = self.0.history.owners.checked_add(1) else {
            self.0.history.complete = false;
            self.0.history.overflowed = true;
            return;
        };
        self.0.history.owners = owners;
        self.0.history.complete &=
            snapshot.consistent() && snapshot.collection == Collection::Finished;
        self.0.history.overflowed |= snapshot.saturated();
        let previous = &mut self.0.snapshot;
        previous.requested |= snapshot.requested;
        previous.configured |= snapshot.configured;
        previous.native_started |= snapshot.native_started;
        previous.cleanup = previous.cleanup.retain(snapshot.cleanup);
        if snapshot.collection != Collection::Finished {
            previous.collection = Collection::Unconfirmed;
        }
        for (total, received) in [
            (
                &mut previous.received_submitted,
                snapshot.received_submitted,
            ),
            (&mut previous.received_unknown, snapshot.received_unknown),
            (&mut previous.received_refused, snapshot.received_refused),
        ] {
            if let Some(value) = total.checked_add(received) {
                *total = value;
            } else {
                *total = u64::MAX;
                self.0.history.overflowed = true;
            }
        }
        self.0.history.overflowed |= previous.saturated();
        self.0.history.complete &= !self.0.history.overflowed;
    }

    /// Include the current original owner once in a read-only completion.
    /// Neither repeated reads nor a failed replacement changes retired evidence.
    pub fn completion(self, current_owner: u64, snapshot: Snapshot) -> Completion {
        let mut finished = self;
        finished.retire(current_owner, snapshot);
        finished.0
    }
}

#[cfg(test)]
mod tests;
