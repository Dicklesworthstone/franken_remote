//! Bounded custody of every table the helper may have installed.
//!
//! Reserve before starting installation. A failed or cancelled command may
//! have committed, so its table keeps that reservation until deletion or
//! absence is confirmed. Taking an orphan for asynchronous cleanup does not
//! free capacity, and dropping that cleanup makes it retryable again.
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    Full,
    Duplicate,
    Poisoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Held,
    Orphan,
    Cleaning,
}

#[derive(Debug)]
struct Entry {
    name: String,
    state: State,
}

#[derive(Debug)]
struct Ledger {
    limit: usize,
    entries: Vec<Entry>,
}

#[derive(Debug, Clone)]
pub(crate) struct Tables(Arc<Mutex<Ledger>>);

impl Tables {
    pub(crate) fn new(limit: usize) -> Self {
        Self(Arc::new(Mutex::new(Ledger {
            limit,
            entries: Vec::with_capacity(limit),
        })))
    }

    /// The caller supplies a validated helper-generated name, never peer text.
    pub(crate) fn reserve(&self, name: String) -> Result<Table, Error> {
        let mut ledger = self.0.lock().map_err(|_| Error::Poisoned)?;
        if ledger.entries.iter().any(|entry| entry.name == name) {
            return Err(Error::Duplicate);
        }
        if ledger.entries.len() >= ledger.limit {
            return Err(Error::Full);
        }
        ledger.entries.push(Entry {
            name: name.clone(),
            state: State::Held,
        });
        Ok(Table {
            name,
            tables: self.clone(),
            may_exist: false,
        })
    }

    /// Cleanup keeps custody even after a poisoned lock; poisoning prevents
    /// new reservations but must never discard already admitted tables.
    fn cleanup(&self) -> MutexGuard<'_, Ledger> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn take_orphans(&self) -> Orphans {
        let mut ledger = self.cleanup();
        let mut names = Vec::new();
        for entry in &mut ledger.entries {
            if entry.state == State::Orphan {
                names.push(entry.name.clone());
                entry.state = State::Cleaning;
            }
        }
        Orphans {
            tables: self.clone(),
            names,
        }
    }
}

/// One non-cloneable reservation, retained by its connection while active or
/// uncertain. Dropping after an install could have run schedules cleanup.
#[derive(Debug)]
pub(crate) struct Table {
    name: String,
    tables: Tables,
    may_exist: bool,
}

impl Table {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Call immediately before submitting any potentially effective install.
    pub(crate) fn begin_install(&mut self) {
        self.may_exist = true;
    }

    /// Only a successful delete or confirmed absence may free installed custody.
    pub(crate) fn removed(mut self) {
        self.may_exist = false;
    }
}

impl Drop for Table {
    fn drop(&mut self) {
        let mut ledger = self.tables.cleanup();
        if self.may_exist {
            if let Some(entry) = ledger
                .entries
                .iter_mut()
                .find(|entry| entry.name == self.name)
            {
                entry.state = State::Orphan;
            }
        } else {
            ledger.entries.retain(|entry| entry.name != self.name);
        }
    }
}

/// An exclusive cleanup claim, still charged to the original table budget.
/// A failed or abandoned cleanup returns its tables to the next janitor turn.
pub(crate) struct Orphans {
    tables: Tables,
    names: Vec<String>,
}

impl Orphans {
    pub(crate) fn names(&self) -> &[String] {
        &self.names
    }

    pub(crate) fn removed(mut self) {
        self.tables
            .cleanup()
            .entries
            .retain(|entry| !self.names.contains(&entry.name));
        self.names.clear();
    }
}

impl Drop for Orphans {
    fn drop(&mut self) {
        for entry in &mut self.tables.cleanup().entries {
            if entry.state == State::Cleaning && self.names.contains(&entry.name) {
                entry.state = State::Orphan;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(n: usize) -> String {
        format!("frdh_{n:032x}")
    }

    fn orphan(tables: &Tables, n: usize) {
        let mut table = tables.reserve(name(n)).unwrap();
        table.begin_install();
    }

    #[test]
    fn failed_cleanups_refuse_new_installs_without_forgetting_any_table() {
        let tables = Tables::new(32);
        for n in 0..32 {
            orphan(&tables, n);
        }
        for n in 32..100 {
            assert_eq!(tables.reserve(name(n)).unwrap_err(), Error::Full);
            // More failures, including after the old orphan list's capacity,
            // never discard custody and never admit another possible rule.
            let retry = tables.take_orphans();
            assert_eq!(retry.names(), (0..32).map(name).collect::<Vec<_>>());
            drop(retry);
        }
        let removed = tables.take_orphans();
        assert_eq!(tables.reserve(name(100)).unwrap_err(), Error::Full);
        removed.removed();
        assert_eq!(tables.take_orphans().names(), Vec::<String>::new());
        assert!(tables.reserve(name(100)).is_ok());
    }

    #[test]
    fn cleanup_in_flight_keeps_capacity_and_cannot_claim_an_active_table() {
        let tables = Tables::new(3);
        let mut active = tables.reserve(name(0)).unwrap();
        assert_eq!(active.name(), name(0));
        active.begin_install();
        orphan(&tables, 1);
        orphan(&tables, 2);
        let batch = tables.take_orphans();
        assert_eq!(batch.names(), [name(1), name(2)]);
        assert_eq!(tables.take_orphans().names(), Vec::<String>::new());
        assert_eq!(tables.reserve(name(3)).unwrap_err(), Error::Full);
        batch.removed();
        assert_eq!(tables.reserve(name(0)).unwrap_err(), Error::Duplicate);
        let replacement = tables.reserve(name(3)).unwrap();
        drop(active);
        let batch = tables.take_orphans();
        assert_eq!(batch.names(), [name(0)]);
        batch.removed();
        drop(replacement);
    }

    #[test]
    fn an_abandoned_install_or_cleanup_keeps_retryable_custody() {
        let tables = Tables::new(2);
        orphan(&tables, 0);
        let first = tables.take_orphans();
        orphan(&tables, 1);
        let second = tables.take_orphans();
        assert_eq!(first.names(), [name(0)]);
        assert_eq!(second.names(), [name(1)]);
        assert_eq!(tables.reserve(name(2)).unwrap_err(), Error::Full);
        // A connection cleanup and the periodic janitor may overlap. Cancelling
        // one returns only its own entries, without stealing the other's claim.
        drop(first);
        let retried = tables.take_orphans();
        assert_eq!(retried.names(), [name(0)]);
        retried.removed();
        assert!(tables.reserve(name(2)).is_ok());
        drop(second);
        assert_eq!(tables.take_orphans().names(), [name(1)]);
    }

    #[test]
    fn a_cancelled_preflight_or_confirmed_remove_frees_its_reservation() {
        let tables = Tables::new(1);
        let preflight = tables.reserve(name(0)).unwrap();
        assert_eq!(tables.reserve(name(1)).unwrap_err(), Error::Full);
        drop(preflight);
        assert_eq!(tables.take_orphans().names(), Vec::<String>::new());
        let mut installed = tables.reserve(name(1)).unwrap();
        installed.begin_install();
        installed.removed();
        assert_eq!(tables.take_orphans().names(), Vec::<String>::new());
        assert!(tables.reserve(name(2)).is_ok());
    }
}
