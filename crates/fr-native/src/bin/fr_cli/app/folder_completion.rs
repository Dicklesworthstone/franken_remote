//! Content-free original folder-job evidence shared by both native app directions.
//! These counters describe confirmed deliveries, never directory equality.
use serde_json::{Value, json};

pub(super) const MAX_FOLDER_PUBLICATIONS: u64 = 512;
pub(super) const MAX_FOLDER_RESULTS: usize = 513;

/// Job observations supplement the publication receipts; they never replace
/// proof of publication or the original disk worker's cleanup result.
pub(super) struct FolderJob {
    pub(super) id: u64,
    pub(super) phase: &'static str,
    pub(super) revision: u64,
    pub(super) rounds_settled: u64,
    pub(super) confirmed_publications: u64,
    pub(super) confirmed_conflicts: Option<u64>,
    pub(super) failure: Option<&'static str>,
    pub(super) cleanup: &'static str,
}

impl FolderJob {
    pub(super) fn parse(value: Option<&Value>) -> Result<Option<Self>, ()> {
        let Some(value) = value else {
            return Ok(None);
        };
        if value.get("evidence_incomplete").is_some() {
            return Err(());
        }
        let phase = value
            .get("phase")
            .and_then(Value::as_str)
            .and_then(phase)
            .ok_or(())?;
        let failure = match value.get("failure") {
            Some(Value::Null) => None,
            Some(Value::String(code)) => Some(sync_failure(code).ok_or(())?),
            _ => return Err(()),
        };
        let job = Self {
            id: value.get("id").and_then(Value::as_u64).ok_or(())?,
            phase,
            revision: value.get("revision").and_then(Value::as_u64).ok_or(())?,
            rounds_settled: value
                .get("rounds_settled")
                .and_then(Value::as_u64)
                .ok_or(())?,
            confirmed_publications: value
                .get("confirmed_publications")
                .and_then(Value::as_u64)
                .ok_or(())?,
            confirmed_conflicts: value
                .get("confirmed_conflicts")
                .map(|count| count.as_u64().ok_or(()))
                .transpose()?,
            failure,
            cleanup: value
                .get("cleanup")
                .and_then(Value::as_str)
                .and_then(cleanup)
                .ok_or(())?,
        };
        if job.id == 0
            || job.rounds_settled > job.revision
            || job
                .confirmed_conflicts
                .is_some_and(|n| n > job.confirmed_publications)
            || job.confirmed_publications > MAX_FOLDER_PUBLICATIONS
            || match job.phase {
                "cancelled" => job.failure != Some("sync_cancelled"),
                "failed" => job.failure.is_none() || job.failure == Some("sync_cancelled"),
                _ => job.failure.is_some(),
            }
        {
            return Err(());
        }
        Ok(Some(job))
    }

    /// A malformed terminal document cannot erase already reported bounded
    /// publication counters. This projection is explicitly incomplete and
    /// never passes as a complete job or positive cleanup evidence.
    pub(super) fn partial(value: Option<&Value>) -> Option<Value> {
        let value = value?.as_object()?;
        let mut retained = json!({"evidence_incomplete": true});
        for key in ["id", "revision", "rounds_settled"] {
            if let Some(number) = value.get(key).and_then(Value::as_u64)
                && (key != "id" || number != 0)
            {
                retained[key] = json!(number);
            }
        }
        for key in ["confirmed_publications", "confirmed_conflicts"] {
            if let Some(number) = value
                .get(key)
                .and_then(Value::as_u64)
                .filter(|number| *number <= MAX_FOLDER_PUBLICATIONS)
            {
                retained[key] = json!(number);
            }
        }
        for (key, decode) in [
            ("phase", phase as fn(&str) -> Option<&'static str>),
            ("cleanup", cleanup as fn(&str) -> Option<&'static str>),
            ("failure", sync_failure as fn(&str) -> Option<&'static str>),
        ] {
            if let Some(code) = value.get(key).and_then(Value::as_str).and_then(decode) {
                retained[key] = json!(code);
            }
        }
        if value.get("failure").is_some_and(Value::is_null) {
            retained["failure"] = Value::Null;
        }
        Some(retained)
    }

    pub(super) fn json(&self) -> Value {
        let mut value = json!({
            "id": self.id,
            "phase": self.phase,
            "revision": self.revision,
            "rounds_settled": self.rounds_settled,
            "confirmed_publications": self.confirmed_publications,
            "failure": self.failure,
            "cleanup": self.cleanup,
        });
        if let Some(count) = self.confirmed_conflicts {
            value["confirmed_conflicts"] = json!(count);
        }
        value
    }
}

fn phase(value: &str) -> Option<&'static str> {
    [
        "advertised",
        "accepted",
        "scanning",
        "transferring",
        "settled",
        "conflict_recorded",
        "cancelled",
        "failed",
    ]
    .into_iter()
    .find(|known| *known == value)
}

fn cleanup(value: &str) -> Option<&'static str> {
    ["finished", "pending", "failed", "unconfirmed"]
        .into_iter()
        .find(|known| *known == value)
}

fn sync_failure(value: &str) -> Option<&'static str> {
    [
        "sync_invalid_config",
        "sync_keep_both_required",
        "sync_not_negotiated",
        "sync_reconnect_refused",
        "sync_job_mismatch",
        "sync_direction_unsupported",
        "sync_state_refused",
        "sync_clock",
        "sync_expired",
        "sync_cancelled",
        "sync_resource_limit",
        "sync_source_refused",
        "sync_transfer_failed",
        "sync_worker_failed",
        "sync_cleanup_unconfirmed",
        "sync_publication_unknown",
        "sync_durability_unconfirmed",
        "sync_transfer_stopped",
        "sync_peer_failed",
    ]
    .into_iter()
    .find(|known| *known == value)
}
