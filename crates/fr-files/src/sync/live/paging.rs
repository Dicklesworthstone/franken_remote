//! Fair bounded selection from an already checked source manifest.
//!
//! A page owns at most one sender batch. Only original peer settlement commits
//! its cursor; scanning, source cleanup or a published prefix cannot move it.
//! The cursor carries no file handle or authority into the next round.
use super::LiveError;
#[cfg(any(unix, test))]
use super::MAX_ENTRIES;
#[cfg(any(unix, test))]
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct Continuation {
    last: Option<String>,
    more: bool,
}

#[cfg(any(unix, test))]
pub(super) struct Page<'a> {
    pub(super) names: Vec<&'a str>,
    pub(super) continuation: Continuation,
}

#[derive(Default)]
pub(super) struct Cursor {
    settled: Option<String>,
    prepared: Option<Continuation>,
}
impl Cursor {
    pub(super) fn after(&self) -> Option<&str> {
        self.settled.as_deref()
    }
    pub(super) fn stage(&mut self, continuation: Continuation) -> Result<(), LiveError> {
        if self.prepared.is_some() {
            return Err(LiveError::WrongPhase);
        }
        self.prepared = Some(continuation);
        Ok(())
    }
    pub(super) fn settle(&mut self) -> Result<bool, LiveError> {
        let continuation = self.prepared.take().ok_or(LiveError::WrongPhase)?;
        if let Some(last) = continuation.last {
            self.settled = Some(last);
        }
        Ok(continuation.more)
    }
    pub(super) fn stop(&mut self) {
        self.prepared = None;
    }
}

/// Input is the bounded manifest's ordered, unique (name, content-hash) pairs.
/// Start after the last settled page, then wrap exactly once. Changed early
/// names cannot indefinitely displace later files from the finite batch.
#[cfg(any(unix, test))]
pub(super) fn select<'a, I>(
    entries: I,
    ledger: &BTreeMap<String, String>,
    after: Option<&str>,
) -> Page<'a>
where
    I: Iterator<Item = (&'a str, &'a str)> + Clone,
{
    let later = entries
        .clone()
        .filter(|(name, _)| after.is_none_or(|after| *name > after));
    let earlier = entries.filter(|(name, _)| after.is_some_and(|after| *name <= after));
    let mut changed = later
        .chain(earlier)
        .filter(|(name, hash)| ledger.get(*name).map(String::as_str) != Some(*hash));
    let names: Vec<_> = changed
        .by_ref()
        .take(MAX_ENTRIES as usize)
        .map(|(name, _)| name)
        .collect();
    let continuation = Continuation {
        last: names.last().map(|name| (*name).to_owned()),
        more: changed.next().is_some(),
    };
    Page {
        names,
        continuation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(count: usize) -> BTreeMap<String, String> {
        (0..count)
            .map(|index| (format!("file-{index:03}"), "original".into()))
            .collect()
    }
    fn plan<'a>(
        entries: &'a BTreeMap<String, String>,
        ledger: &BTreeMap<String, String>,
        cursor: &Cursor,
    ) -> Page<'a> {
        select(
            entries
                .iter()
                .map(|(name, hash)| (name.as_str(), hash.as_str())),
            ledger,
            cursor.after(),
        )
    }
    fn acknowledge(
        entries: &BTreeMap<String, String>,
        names: &[&str],
        ledger: &mut BTreeMap<String, String>,
    ) {
        for &name in names {
            ledger.insert(name.into(), entries[name].clone());
        }
    }

    #[test]
    fn complete_manifest_progresses_through_bounded_settled_pages() {
        let entries = versions(super::super::MAX_SOURCE_ENTRIES as usize);
        let mut ledger = BTreeMap::new();
        let mut cursor = Cursor::default();
        let mut pages = 0;
        loop {
            let page = plan(&entries, &ledger, &cursor);
            assert_eq!(page.names.len(), MAX_ENTRIES as usize);
            assert!(page.names.iter().all(|name| !ledger.contains_key(*name)));
            acknowledge(&entries, &page.names, &mut ledger);
            let original_cursor = cursor.after().map(str::to_owned);
            cursor.stage(page.continuation).unwrap();
            assert_eq!(cursor.after(), original_cursor.as_deref());
            pages += 1;
            let more = cursor.settle().unwrap();
            if !more {
                break;
            }
        }
        assert_eq!(pages, 8);
        assert_eq!(ledger.len(), entries.len());
        let empty = plan(&entries, &ledger, &cursor);
        assert!(empty.names.is_empty());
        cursor.stage(empty.continuation).unwrap();
        assert!(!cursor.settle().unwrap());
        assert_eq!(cursor.after(), Some("file-255"));
    }

    #[test]
    fn changing_early_names_cannot_starve_later_original_versions() {
        let mut entries = versions(96);
        let mut ledger = BTreeMap::new();
        let mut cursor = Cursor::default();
        let first = plan(&entries, &ledger, &cursor);
        acknowledge(&entries, &first.names, &mut ledger);
        cursor.stage(first.continuation).unwrap();
        assert!(cursor.settle().unwrap());
        for (round, expected_first) in [(1, "file-032"), (2, "file-064")] {
            for index in 0..32 {
                entries.insert(format!("file-{index:03}"), format!("edit-{round}"));
            }
            let page = plan(&entries, &ledger, &cursor);
            assert_eq!(page.names.first().copied(), Some(expected_first));
            assert_eq!(page.names.len(), 32);
            acknowledge(&entries, &page.names, &mut ledger);
            cursor.stage(page.continuation).unwrap();
            assert!(cursor.settle().unwrap());
        }
        let wrapped = plan(&entries, &ledger, &cursor);
        assert_eq!(wrapped.names.first().copied(), Some("file-000"));
        assert_eq!(wrapped.names.last().copied(), Some("file-031"));
        acknowledge(&entries, &wrapped.names, &mut ledger);
        cursor.stage(wrapped.continuation).unwrap();
        assert!(!cursor.settle().unwrap());
        assert_eq!(ledger, entries);
    }

    #[test]
    fn acknowledged_prefix_does_not_consume_page_capacity() {
        let entries = versions(65);
        let mut ledger: BTreeMap<_, _> = entries
            .iter()
            .take(32)
            .map(|(name, hash)| (name.clone(), hash.clone()))
            .collect();
        let mut cursor = Cursor::default();
        let page = plan(&entries, &ledger, &cursor);
        assert_eq!(page.names.first().copied(), Some("file-032"));
        assert_eq!(page.names.last().copied(), Some("file-063"));
        acknowledge(&entries, &page.names, &mut ledger);
        cursor.stage(page.continuation).unwrap();
        assert!(cursor.settle().unwrap());
        let tail = plan(&entries, &ledger, &cursor);
        assert_eq!(tail.names, ["file-064"]);
        cursor.stage(tail.continuation).unwrap();
        assert!(!cursor.settle().unwrap());
    }

    #[test]
    fn unconfirmed_or_abandoned_pages_never_advance_the_cursor() {
        let entries = versions(64);
        let ledger = BTreeMap::new();
        let mut cursor = Cursor::default();
        assert_eq!(cursor.settle(), Err(LiveError::WrongPhase));
        let page = plan(&entries, &ledger, &cursor);
        cursor.stage(page.continuation).unwrap();
        assert_eq!(cursor.after(), None);
        let replacement = plan(&entries, &ledger, &cursor);
        assert_eq!(
            cursor.stage(replacement.continuation),
            Err(LiveError::WrongPhase)
        );
        cursor.stop();
        assert_eq!(cursor.after(), None);
        assert_eq!(cursor.settle(), Err(LiveError::WrongPhase));
        let retry = plan(&entries, &ledger, &cursor);
        assert_eq!(retry.names.first().copied(), Some("file-000"));
    }

    #[test]
    fn deleted_cursor_and_new_names_wrap_without_duplicates() {
        let mut entries = versions(40);
        let mut ledger = BTreeMap::new();
        let mut cursor = Cursor::default();
        let first = plan(&entries, &ledger, &cursor);
        acknowledge(&entries, &first.names, &mut ledger);
        cursor.stage(first.continuation).unwrap();
        assert!(cursor.settle().unwrap());
        entries.remove("file-031");
        entries.insert("before".into(), "new".into());
        entries.insert("later".into(), "new".into());
        let page = plan(&entries, &ledger, &cursor);
        assert_eq!(page.names.len(), 10);
        assert_eq!(page.names.first().copied(), Some("file-032"));
        assert_eq!(page.names[8], "later");
        assert_eq!(page.names[9], "before");
        cursor.stage(page.continuation).unwrap();
        assert!(!cursor.settle().unwrap());
        assert_eq!(cursor.after(), Some("before"));
    }

    #[test]
    fn exact_batch_has_no_spurious_continuation() {
        let entries = versions(MAX_ENTRIES as usize);
        let mut cursor = Cursor::default();
        let page = plan(&entries, &BTreeMap::new(), &cursor);
        assert_eq!(page.names.len(), MAX_ENTRIES as usize);
        cursor.stage(page.continuation).unwrap();
        assert!(!cursor.settle().unwrap());
    }
}
