//! Separate reports for fresh explicit choices; original grant evidence is immutable.
use super::{Capabilities, CleanupSummary, Counters, Failure, InputOutcome, ResultEvent};
use std::fmt::Write as _;

/// Both the CLI parser and native reconnect policy cap attempts at 32.
pub(super) const MAX_ATTEMPTS: usize = 32;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fresh {
    pub(super) attempt: u8,
    pub(super) granted: bool,
    pub(super) capabilities: Option<Capabilities>,
    pub(super) results: u64,
    pub(super) submitted: u64,
    pub(super) suspensions: u32,
    pub(super) suspended_us: u64,
    pub(super) dropped_aged: u64,
    pub(super) ended: Option<Result<(), Failure>>,
    pub(super) cleanup: Option<CleanupSummary>,
}
impl Fresh {
    pub(super) fn new(attempt: u8) -> Self {
        Self {
            attempt,
            ..Self::default()
        }
    }
    pub(super) fn result(&mut self, event: &ResultEvent) {
        if let ResultEvent::Completed(result) | ResultEvent::Pointer(result) = event {
            self.results = self.results.saturating_add(1);
            if matches!(result.outcome, InputOutcome::SubmittedToOs) {
                self.submitted = self.submitted.saturating_add(1);
            }
        }
    }
    fn end(&self) -> Option<&'static str> {
        self.ended
            .map(|result| result.err().map_or("completed", super::super::end_code))
    }
    fn exchange(&self) -> Option<crate::closure::Report> {
        self.cleanup
            .and_then(|s| {
                s.disconnect
                    .or_else(|| s.control_disconnect.map(|o| o.exchange))
            })
            .map(crate::closure::Report::from)
    }
    fn lease(&self) -> Option<fr_wire::lease_revoked::Revoked> {
        self.cleanup
            .and_then(|s| s.control_disconnect)
            .and_then(|o| o.revocation)
    }
    fn json(&self) -> String {
        let capabilities = self.capabilities.map_or_else(
            || "null".to_owned(),
            |granted| {
                format!(
                    "[{}]",
                    super::capability_names(granted)
                        .iter()
                        .map(|name| crate::output::quoted(name))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            },
        );
        format!(
            "{{\"attempt\":{},\"requested\":true,\"granted\":{},\"capabilities\":{capabilities},\"input_results\":{},\"input_submitted_to_os\":{},\"input_suspensions\":{},\"input_suspended_ms\":{},\"input_dropped_aged\":{},\"end\":{},\"close_exchange\":{},\"lease_report\":{}}}",
            self.attempt,
            self.granted,
            self.results,
            self.submitted,
            self.suspensions,
            self.suspended_us / 1_000,
            self.dropped_aged,
            self.end()
                .map_or_else(|| "null".to_owned(), crate::output::quoted),
            self.exchange()
                .map_or_else(|| "null".to_owned(), crate::closure::Report::json),
            crate::terminal::lease_json(self.lease()),
        )
    }
}
impl Counters {
    pub(crate) fn fresh_history(&self) -> crate::ControlHistory {
        let mut text = String::new();
        let mut entries = Vec::new();
        for fresh in self.fresh.iter().flatten() {
            entries.push(fresh.json());
            let _ = write!(
                text,
                " Explicit control request on attempt {}: {}; {} host input result(s), {} submitted to the host OS.",
                fresh.attempt,
                if fresh.granted {
                    "granted"
                } else {
                    "not granted"
                },
                fresh.results,
                fresh.submitted,
            );
            if let Some(end) = fresh.end() {
                let _ = write!(text, " Attempt ended: {end}.");
            }
            if let Some(exchange) = fresh.exchange() {
                let _ = write!(text, " Close: {}", exchange.text());
            }
            if let Some(lease) = fresh.lease() {
                let _ = write!(
                    text,
                    " Lease report: {}.",
                    crate::terminal::lease_text(lease)
                );
            }
        }
        crate::ControlHistory {
            json: format!(",\"explicit_control_requests\":[{}]", entries.join(",")),
            text,
        }
    }
}
