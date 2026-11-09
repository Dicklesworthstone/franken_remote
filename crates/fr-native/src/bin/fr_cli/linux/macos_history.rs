//! Retain the original close/effect reports across explicit display changes.
//! A later picker cancellation, dial failure or successful session cannot erase
//! what the preceding host actually reported about cleanup and input effects.
use super::{Failure, Report};
use std::fmt::Write as _;

fn history(report: &Report) -> crate::ControlHistory {
    let mut entries = String::new();
    let mut text = String::new();
    for change in &report.display_change_closes {
        if !entries.is_empty() {
            entries.push(',');
        }
        let close = change.close_exchange.map(crate::closure::Report::from);
        let _ = write!(
            entries,
            "{{\"attempt\":{},\"close_exchange\":{},\"lease_report\":{}}}",
            change.attempt,
            close.map_or_else(|| "null".to_owned(), crate::closure::Report::json),
            crate::terminal::lease_json(change.lease_revocation),
        );
        let _ = write!(
            text,
            " Display change after attempt {}: {}",
            change.attempt,
            close.map_or_else(
                || "No completed close exchange; host cleanup remains unknown.".to_owned(),
                crate::closure::Report::text,
            ),
        );
        if let Some(lease) = change.lease_revocation {
            let _ = write!(
                text,
                " Lease report: {}.",
                crate::terminal::lease_text(lease)
            );
        }
    }
    crate::ControlHistory {
        json: format!(
            ",\"display_changes\":{},\"display_change_closes\":[{entries}]",
            report.display_changes,
        ),
        text,
    }
}

fn preserve(history: &crate::ControlHistory, failure: &mut Failure) {
    let (mut json, mut text) = failure.control_history.map_or_else(
        || (String::new(), String::new()),
        |previous| (previous.json.clone(), previous.text.clone()),
    );
    json.push_str(&history.json);
    text.push_str(&history.text);
    failure.control_history = Some(Box::leak(Box::new(crate::ControlHistory { json, text })));
}

pub(super) fn finish(
    report: &Report,
    result: Result<String, Failure>,
    json: bool,
) -> Result<String, Failure> {
    if report.display_change_closes.is_empty() {
        return result;
    }
    let history = history(report);
    match result {
        Err(mut failure) => {
            preserve(&history, &mut failure);
            Err(failure)
        }
        Ok(text) if json => {
            let Some(prefix) = text.trim_end().strip_suffix('}') else {
                let mut failure = Failure::new(
                    "native_output_invalid",
                    "The session completion could not be formatted; earlier display-change cleanup and effect reports remain available in this error.",
                    1,
                );
                preserve(&history, &mut failure);
                return Err(failure);
            };
            Ok(format!("{prefix}{}}}\n", history.json))
        }
        Ok(mut text) => {
            text.push_str(history.text.trim_start());
            text.push('\n');
            Ok(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_native::macos::session::DisplayChangeReport;

    fn changed() -> Report {
        Report {
            display_changes: 1,
            display_change_closes: vec![DisplayChangeReport {
                attempt: 1,
                close_exchange: None,
                lease_revocation: None,
            }],
            ..Report::default()
        }
    }

    #[test]
    fn a_later_failure_keeps_prior_cleanup_unknown_and_other_evidence() {
        let mut failure = Failure::new("display_choice_cancelled", "Cancelled.", 130);
        failure.control_history = Some(Box::leak(Box::new(crate::ControlHistory {
            json: ",\"files_requested\":1".to_owned(),
            text: " Earlier file receipt.".to_owned(),
        })));
        let error = finish(&changed(), Err(failure), true).unwrap_err();
        assert_eq!(error.code, "display_choice_cancelled");
        assert_eq!(error.exit, 130);
        let history = error.control_history.unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&format!("{{\"outcome\":\"error\"{}}}", history.json)).unwrap();
        assert_eq!(value["files_requested"], 1);
        assert_eq!(value["display_changes"], 1);
        assert_eq!(value["display_change_closes"][0]["attempt"], 1);
        assert!(value["display_change_closes"][0]["close_exchange"].is_null());
        assert!(history.text.contains("host cleanup remains unknown"));
        assert!(history.text.contains("Earlier file receipt"));
    }

    #[test]
    fn successful_completion_does_not_replace_earlier_effect_evidence() {
        let text = finish(
            &changed(),
            Ok("{\"outcome\":\"success\"}\n".to_owned()),
            true,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["outcome"], "success");
        assert_eq!(value["display_changes"], 1);
        assert!(value["display_change_closes"][0]["lease_report"].is_null());
    }
}
