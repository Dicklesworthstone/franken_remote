//! One authenticated Linux LocalAPI query, projected through the shared parser.
use super::{LocalApi, Lookup, bounded, now, wall_now};
use crate::{Discovery, Error, metadata};
use asupersync::cx::Cx;
use std::{sync::atomic::Ordering, time::Duration};

impl LocalApi {
    /// Enumerate candidates using ONE credential-checked installed-daemon read.
    /// This shares the existing lookup exclusion and fixed one-second budget.
    /// Invalid/ambiguous/expired/shared candidates are counted and omitted.
    /// The result cannot attach, dial or authorize a session.
    pub async fn discover(&self, cx: &Cx) -> Result<Discovery, Error> {
        let started_us = now(cx)?;
        let started_wall = wall_now()?;
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let _lookup = Lookup(self.busy.clone());
        Box::pin(bounded(cx, Duration::from_secs(1), async {
            let (bytes, _) = self
                .get("/localapi/v0/status?peers=true", metadata::STATUS_BYTES)
                .await?;
            Discovery::from_status_json(&bytes, started_us, started_wall, || now(cx))
        }))
        .await
    }
}

#[cfg(test)]
#[path = "tests/discovery.rs"]
mod tests;
