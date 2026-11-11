//! Content-free media and smoothness completion of the Mac viewer: the last
//! attempt's decoder/recovery/freshness counters (the same object the Linux
//! client reports) and the gaps between locally submitted pictures. Counts and
//! bucket bounds only; neither proves delivery, visibility or latency.
use super::{Failure, Report};
use fr_native::macos::cadence::FrameCadence;

fn number(value: Option<u64>) -> String {
    value.map_or_else(|| "null".to_owned(), |value| value.to_string())
}

fn tenths(value: Option<u64>) -> String {
    value.map_or_else(
        || "null".to_owned(),
        |value| format!("{}.{}", value / 10, value % 10),
    )
}

fn cadence_json(cadence: &FrameCadence) -> String {
    format!(
        "{{\"gaps\":{},\"submitted_per_second_mean\":{},\"p50_ms_at_most\":{},\"p95_ms_at_most\":{},\"p99_ms_at_most\":{},\"max_ms\":{},\"over_50ms\":{},\"over_250ms\":{}}}",
        cadence.gaps(),
        tenths(cadence.mean_per_second_tenths()),
        number(cadence.quantile_upper_ms(500)),
        number(cadence.quantile_upper_ms(950)),
        number(cadence.quantile_upper_ms(990)),
        number(cadence.max_ms()),
        cadence.hitches(),
        cadence.stalls(),
    )
}

fn cadence_text(cadence: &FrameCadence) -> String {
    let (Some(p50), Some(p95), Some(p99), Some(max), Some(mean)) = (
        cadence.quantile_upper_ms(500),
        cadence.quantile_upper_ms(950),
        cadence.quantile_upper_ms(990),
        cadence.max_ms(),
        cadence.mean_per_second_tenths(),
    ) else {
        return " Frame cadence: fewer than two pictures in any window.".to_owned();
    };
    format!(
        " Frame cadence over {} gaps between submitted pictures: mean {}/s; at most {p50} ms (p50), {p95} ms (p95), {p99} ms (p99); longest {max} ms; {} over 50 ms, {} over 250 ms. Submissions, not displayed refreshes; an unchanged host screen sends fewer pictures.",
        cadence.gaps(),
        tenths(Some(mean)),
        cadence.hitches(),
        cadence.stalls(),
    )
}

fn media_text(report: &Report) -> String {
    let Some(media) = report.media.as_ref() else {
        return " Media: no viewer opened in the last attempt.".to_owned();
    };
    let age = media
        .presented_age
        .quantile_upper_us(500)
        .zip(media.presented_age.quantile_upper_us(950))
        .map_or_else(String::new, |(p50, p95)| {
            format!(
                " Presented source age at most {} ms (p50), {} ms (p95) over {} reports.",
                p50 / 1000,
                p95 / 1000,
                media.presented_age.reports(),
            )
        });
    format!(
        " Media (last attempt): {} decoded, {} repair request(s), {} recovered stream(s).{age}{}",
        media.decoded,
        media.repair_requests,
        media.recovered_streams,
        super::super::quality::text(Some(media)),
    )
}

fn evidence(report: &Report) -> crate::ControlHistory {
    crate::ControlHistory {
        json: format!(
            ",\"last_attempt_media\":{},\"frame_intervals\":{}",
            super::super::media_json(report.media.as_ref()),
            cadence_json(&report.frame_intervals),
        ),
        text: format!(
            "{}{}",
            media_text(report),
            cadence_text(&report.frame_intervals)
        ),
    }
}

fn preserve(report: &Report, failure: &mut Failure) {
    if failure
        .control_history
        .is_some_and(|previous| previous.json.contains(",\"frame_intervals\":"))
    {
        return;
    }
    let next = evidence(report);
    let (mut json, mut text) = failure.control_history.map_or_else(
        || (String::new(), String::new()),
        |previous| (previous.json.clone(), previous.text.clone()),
    );
    json.push_str(&next.json);
    text.push_str(&next.text);
    failure.control_history = Some(Box::leak(Box::new(crate::ControlHistory { json, text })));
}

pub(super) fn finish(
    report: &Report,
    result: Result<String, Failure>,
    json: bool,
) -> Result<String, Failure> {
    match result {
        Err(mut failure) => {
            preserve(report, &mut failure);
            Err(failure)
        }
        Ok(text) if json => {
            let Some(prefix) = text.trim_end().strip_suffix('}') else {
                let mut failure = Failure::new(
                    "native_output_invalid",
                    "The session completion could not be formatted; the media and frame-cadence observations remain available in this error.",
                    1,
                );
                preserve(report, &mut failure);
                return Err(failure);
            };
            Ok(format!("{prefix}{}}}\n", evidence(report).json))
        }
        Ok(mut text) => {
            text.push_str(evidence(report).text.trim_start());
            text.push('\n');
            Ok(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(report: &Report) -> serde_json::Value {
        serde_json::from_str(&format!(
            "{{\"outcome\":\"success\"{}}}",
            evidence(report).json
        ))
        .unwrap()
    }

    #[test]
    fn an_unopened_attempt_reports_null_media_and_empty_cadence() {
        let report = Report::default();
        let value = value(&report);
        assert!(value["last_attempt_media"].is_null());
        let cadence = &value["frame_intervals"];
        assert_eq!(cadence["gaps"], 0);
        for key in [
            "submitted_per_second_mean",
            "p50_ms_at_most",
            "p95_ms_at_most",
            "p99_ms_at_most",
            "max_ms",
        ] {
            assert!(cadence[key].is_null(), "{key}: {cadence}");
        }
        assert_eq!(
            (
                cadence["over_50ms"].as_u64(),
                cadence["over_250ms"].as_u64()
            ),
            (Some(0), Some(0))
        );
        let text = evidence(&report).text;
        assert!(text.contains("no viewer opened"), "{text}");
        assert!(text.contains("fewer than two pictures"), "{text}");
    }

    #[test]
    fn opened_media_and_a_hitch_are_both_reported() {
        let mut report = Report {
            media: Some(frd::session_startup::ViewerStatistics {
                decoded: 41,
                repair_requests: 3,
                recovered_streams: 1,
                ..Default::default()
            }),
            ..Report::default()
        };
        for _ in 0..98 {
            report.frame_intervals.record(16_667);
        }
        report.frame_intervals.record(80_000);
        report.frame_intervals.record(1_200_000);
        let value = value(&report);
        let media = &value["last_attempt_media"];
        assert_eq!(media["decoded"], 41);
        assert_eq!(media["repair_requests"], 3);
        assert_eq!(media["recovered_streams"], 1);
        assert!(media["presented_age_p50_ms_at_most"].is_null());
        let cadence = &value["frame_intervals"];
        assert_eq!(cadence["gaps"], 100);
        assert_eq!(cadence["p50_ms_at_most"], 17);
        assert_eq!(cadence["p99_ms_at_most"], 81);
        assert_eq!(cadence["max_ms"], 1_200);
        assert_eq!(cadence["over_50ms"], 2);
        assert_eq!(cadence["over_250ms"], 1);
        // 100 gaps over 2.913 s.
        assert_eq!(cadence["submitted_per_second_mean"], 34.3);
        let text = evidence(&report).text;
        assert!(text.contains("41 decoded, 3 repair request(s)"), "{text}");
        assert!(
            text.contains("longest 1200 ms; 2 over 50 ms, 1 over 250 ms"),
            "{text}"
        );
    }

    #[test]
    fn success_json_gains_the_fields_and_failures_keep_them_once() {
        let report = Report::default();
        let done = finish(&report, Ok("{\"outcome\":\"success\"}\n".to_owned()), true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&done).unwrap();
        assert_eq!(parsed["frame_intervals"]["gaps"], 0);

        let failure = Failure::new("cancelled", "stopped", 130);
        let Err(failure) = finish(&report, Err(failure), true) else {
            panic!("a failure stays a failure");
        };
        let Err(failure) = finish(&report, Err(failure), true) else {
            panic!("a failure stays a failure");
        };
        let history = failure.control_history.unwrap();
        assert_eq!(history.json.matches(",\"frame_intervals\":").count(), 1);
        assert_eq!(failure.exit, 130);

        let Err(failure) = finish(&report, Ok("not json".to_owned()), true) else {
            panic!("unformattable output is refused");
        };
        assert_eq!(failure.code, "native_output_invalid");
    }
}
