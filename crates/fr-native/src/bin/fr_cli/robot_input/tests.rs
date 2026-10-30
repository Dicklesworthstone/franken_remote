//! Exercise the real CLI translation and result envelope without opening a
//! socket, inventing native readiness, or sending input to any desktop.
use super::*;
use fr_client::robot::{
    AcknowledgementStage,
    input::native::{End, Outcome},
};
use std::path::Path;

const CONNECTION: &str = "0123456789abcdef0123456789abcdef";
const LEASE: &str = "fedcba9876543210fedcba9876543210";
const NATIVE: &str = "--experimental-native --display only --worker /fixture/fr-media-worker";

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}
fn action(flag: &str, value: &str) -> Vec<String> {
    words(&format!(
        "robot input n-private-target --connection-id {CONNECTION} --lease {LEASE} \
         --request-id 9 --observation-id 7 --max-observation-age-ms 250 {flag} {value}"
    ))
}
fn replace_value(args: &mut [String], flag: &str, value: &str) {
    let index = args.iter().position(|arg| arg == flag).unwrap();
    args[index + 1] = value.to_owned();
}
fn refused(args: &[String]) -> Failure {
    let failure = command(args).err().expect("must refuse before any I/O");
    assert_eq!(failure.code, "invalid_arguments");
    assert_eq!(failure.exit, 2);
    assert!(!failure.unknown_effect);
    failure
}
fn complete_report() -> Report {
    Report {
        request_id: 9,
        actions_total: 2,
        actions_encoded: 2,
        actions_submitted: 2,
        acknowledged_actions: 2,
        unacknowledged_actions: 0,
        submitted_operations: 4,
        stage: Some(AcknowledgementStage::SubmittedToOs),
        disposition: InputDisposition::Committed,
        host_outcome: Some(Outcome::SubmittedToOs),
        host_reason: None,
        unknown_next_operation: false,
        end: End::HostResult,
    }
}

#[test]
fn explicit_automation_open_preserves_target_trust_display_and_original_owner_limits() {
    let args = words(
        "robot session open host.fixture.ts.net --foreground --role control --accept-input \
         --experimental-native --display 340282366920938463463374607431768211455 \
         --worker /fixture/worker --trust-roots /fixture/roots.pem \
         --socket /fixture/tailscaled.sock --by-name --ipv6 --port 9443 \
         --x-display :7 --fit 960x540 --json",
    );
    assert!(requested(&args));
    let parsed = opening(&args).unwrap();
    assert!(parsed.json);
    assert_eq!(
        parsed.socket.as_deref(),
        Some(Path::new("/fixture/tailscaled.sock"))
    );
    let options::Command::Connect(connection) = parsed.command else {
        panic!("automation must use the original canonical connection");
    };
    assert_eq!(connection.target.node, "host.fixture.ts.net");
    assert_eq!(connection.target.roots, Path::new("/fixture/roots.pem"));
    assert_eq!(connection.target.port, 9443);
    assert!(connection.target.by_name && connection.target.ipv6);
    assert_eq!(
        connection.display,
        options::DisplayChoice::Handle(u128::MAX)
    );
    assert_eq!(connection.worker, Path::new("/fixture/worker"));
    assert_eq!(connection.x_display.as_deref(), Some(":7"));
    assert_eq!(connection.fit_window, Some((960, 540)));
    assert!(connection.control);
    assert_eq!(connection.attempts, 1);
    assert!(!connection.clipboard && !connection.capture_system_shortcuts);
    assert!(connection.audio.is_none());
    assert!(connection.upload.paths.is_empty() && !connection.upload.file_drop);
    assert!(connection.receive.is_none());
}

#[test]
fn accepting_input_requires_explicit_foreground_control_and_native_qualification() {
    for prefix in [
        "--foreground --role view --accept-input",
        "--foreground --role admin --accept-input",
        "--role control --accept-input",
        "--foreground --role control",
        "--accept-input --foreground --role control",
    ] {
        assert!(
            opening(&words(&format!(
                "robot session open n-host {prefix} {NATIVE}"
            )))
            .is_err()
        );
    }
    for (suffix, expected) in [
        ("", "native_transport_unqualified"),
        ("--experimental-native", "invalid_arguments"),
        (
            "--experimental-native --display choose",
            "invalid_arguments",
        ),
        (
            "--experimental-native --display only --trust-roots relative",
            "invalid_arguments",
        ),
    ] {
        let failure = opening(&words(&format!(
            "robot session open n-host --foreground --role control --accept-input {suffix}"
        )))
        .err()
        .expect("native prerequisites must remain enforced");
        assert_eq!(failure.code, expected);
    }
}

#[test]
fn automation_open_cannot_enable_reconnect_auxiliary_lanes_or_duplicate_options() {
    for suffix in [
        "--attempts 2",
        "--clipboard",
        "--no-clipboard",
        "--capture-system-shortcuts",
        "--audio",
        "--send /private/selection",
        "--receive /private/destination",
        "--file-drop",
        "--lease private-lease",
        "--detach",
        "--view-only",
        "--control",
        "--foreground",
        "--accept-input",
        "--role control",
        "--json --json",
        "--display only",
        "--experimental-native",
        "--worker /other/worker",
    ] {
        let failure = opening(&words(&format!(
            "robot session open n-private-target --foreground --role control --accept-input {NATIVE} {suffix}"
        ))).err().expect("duplicate or additional authority must refuse");
        assert_eq!(failure.code, "invalid_arguments");
        let rendered = format!("{failure:?}");
        for private in ["n-private-target", "private-lease", "/private/"] {
            assert!(!rendered.contains(private));
        }
    }
}

#[test]
fn bounded_primitives_keep_signed_host_coordinates_and_physical_key_positions() {
    for (flag, value, expected, count) in [
        (
            "--scroll",
            "-1920,-1080,-120,120",
            Operation::Scroll {
                x: -1920,
                y: -1080,
                dx: -120,
                dy: 120,
            },
            1,
        ),
        (
            "--click",
            "5,-2147483648,2147483647",
            Operation::Click {
                button: 5,
                x: i32::MIN,
                y: i32::MAX,
            },
            2,
        ),
        ("--key", "4", Operation::KeyTap { usage: 4 }, 2),
        ("--key", "164", Operation::KeyTap { usage: 164 }, 2),
        ("--key", "224", Operation::KeyTap { usage: 224 }, 2),
        ("--key", "231", Operation::KeyTap { usage: 231 }, 2),
    ] {
        let args = action(flag, value);
        assert!(requested(&args));
        let parsed = command(&args).unwrap();
        assert_eq!(parsed.target, "n-private-target");
        assert_eq!(parsed.connection, CONNECTION);
        assert!(!parsed.json);
        let request = parsed.request.unwrap();
        assert_eq!(request.lease, id(LEASE).unwrap());
        assert_eq!(request.request_id, 9);
        assert_eq!(request.observation_id, 7);
        assert_eq!(request.max_observation_age_ms, 250);
        assert!(request.operation == expected);
        assert_eq!(request.operation.count(), count);
        assert!(request.valid());
    }
}

#[test]
fn numeric_action_bounds_and_unsupported_text_are_refused_before_dispatch() {
    for (flag, value) in [
        ("--scroll", "0,0,0,0"),
        ("--scroll", "0,0,121,0"),
        ("--scroll", "0,0,0,-121"),
        ("--scroll", "0,0,1"),
        ("--scroll", "2147483648,0,0,1"),
        ("--scroll", "0,0,0,1,2"),
        ("--click", "0,0,0"),
        ("--click", "6,0,0"),
        ("--click", "256,0,0"),
        ("--click", "-1,0,0"),
        ("--click", "1,-2147483649,0"),
        ("--key", "0"),
        ("--key", "3"),
        ("--key", "165"),
        ("--key", "223"),
        ("--key", "232"),
        ("--key", "65536"),
        ("--key", "-1"),
        ("--key", "4,5"),
        ("--key", "private-keystroke"),
        ("--text", "private-typed-text"),
        ("--mouse-down", "1,0,0"),
    ] {
        let failure = refused(&action(flag, value));
        assert!(!format!("{failure:?}").contains("private-"));
    }
}

#[test]
fn input_requires_every_original_identity_and_valid_observation_age() {
    for flag in [
        "--connection-id",
        "--lease",
        "--request-id",
        "--observation-id",
        "--max-observation-age-ms",
        "--click",
    ] {
        let mut args = action("--click", "1,0,0");
        let index = args.iter().position(|arg| arg == flag).unwrap();
        args.drain(index..index + 2);
        refused(&args);
    }
    for (flag, value) in [
        ("--connection-id", "00000000000000000000000000000000"),
        ("--lease", "00000000000000000000000000000000"),
        ("--lease", "FEDCBA9876543210FEDCBA9876543210"),
        ("--lease", "private-bearer"),
        ("--connection-id", "too-short"),
        ("--request-id", "0"),
        ("--request-id", "18446744073709551616"),
        ("--observation-id", "0"),
        ("--observation-id", "-1"),
        ("--max-observation-age-ms", "0"),
        ("--max-observation-age-ms", "1001"),
        ("--max-observation-age-ms", "4294967296"),
    ] {
        let mut args = action("--click", "1,0,0");
        replace_value(&mut args, flag, value);
        let failure = refused(&args);
        for private in ["n-private-target", "private-bearer", LEASE, CONNECTION] {
            assert!(!format!("{failure:?}").contains(private));
        }
    }
    let mut largest = action("--scroll", "-2147483648,2147483647,-120,120");
    replace_value(&mut largest, "--request-id", "18446744073709551615");
    replace_value(&mut largest, "--observation-id", "18446744073709551615");
    replace_value(&mut largest, "--max-observation-age-ms", "1000");
    let request = command(&largest).unwrap().request.unwrap();
    assert_eq!(request.request_id, u64::MAX);
    assert_eq!(request.observation_id, u64::MAX);
    assert!(serde_json::to_vec(&request).unwrap().len() <= 512);
}

#[test]
fn status_cannot_smuggle_actions_or_replace_original_preconditions() {
    let base = format!("robot input n-host --connection-id {CONNECTION} --status --json");
    let parsed = command(&words(&base)).unwrap();
    assert!(parsed.request.is_none() && parsed.json);
    for suffix in [
        "--lease fedcba9876543210fedcba9876543210",
        "--request-id 9",
        "--observation-id 7",
        "--max-observation-age-ms 250",
        "--key 4",
        "--chord 224,4",
        "--status",
        "--json",
    ] {
        refused(&words(&format!("{base} {suffix}")));
    }
    for suffix in [
        "--key 4",
        "--chord 224,4",
        "--scroll 0,0,0,1",
        "--click 1,0,0",
        "--request-id 10",
        "--observation-id 8",
        "--lease fedcba9876543210fedcba9876543210",
        "--connection-id 0123456789abcdef0123456789abcdef",
        "--json --json",
        "--status",
        "--unknown private-payload",
        "--detach",
    ] {
        let mut args = action("--click", "1,0,0");
        args.extend(words(suffix));
        refused(&args);
    }
}

#[test]
fn argument_bounds_control_characters_and_missing_values_never_echo_payloads() {
    let valid = action("--key", "4");
    let mut too_many = valid.clone();
    too_many.resize(33, "private-extra".into());
    refused(&too_many);
    let overlong_target = "x".repeat(254);
    for target in [
        "",
        "--private-option",
        overlong_target.as_str(),
        "private-target\nsecret",
    ] {
        let mut args = valid.clone();
        args[2] = target.into();
        let failure = refused(&args);
        assert!(!format!("{failure:?}").contains("private-target"));
    }
    for value in ["x".repeat(4097), "private-text\tsecret".to_owned()] {
        let mut args = valid.clone();
        replace_value(&mut args, "--key", &value);
        refused(&args);
    }
    let mut missing = valid;
    missing.pop();
    refused(&missing);
}

#[test]
fn wire_requests_and_responses_reject_unknown_fields_and_unbounded_numbers() {
    let request = command(&action("--click", "1,0,0"))
        .unwrap()
        .request
        .unwrap();
    let mut value = serde_json::to_value(request).unwrap();
    value["text"] = "private-text".into();
    assert!(serde_json::from_value::<Request>(value).is_err());
    let mut value = serde_json::to_value(request).unwrap();
    value["operation"]["repeat"] = 5000.into();
    assert!(serde_json::from_value::<Request>(value).is_err());
    let mut value = serde_json::to_value(request).unwrap();
    value["operation"]["button"] = 256.into();
    assert!(serde_json::from_value::<Request>(value).is_err());
    let mut value = serde_json::to_value(Response::Result {
        report: complete_report(),
    })
    .unwrap();
    value["unexpected_authority"] = true.into();
    assert!(serde_json::from_value::<Response>(value).is_err());
    let mut value = serde_json::to_value(Response::Result {
        report: complete_report(),
    })
    .unwrap();
    value["report"]["ticket"] = "private-ticket".into();
    assert!(serde_json::from_value::<Response>(value).is_err());
}

#[test]
fn reply_outcomes_and_exit_codes_preserve_confirmed_prefixes_and_uncertainty() {
    let complete = complete_report();
    let partial = Report {
        actions_encoded: 1,
        actions_submitted: 1,
        acknowledged_actions: 1,
        submitted_operations: 2,
        disposition: InputDisposition::Partial,
        end: End::Expired,
        ..complete
    };
    let unknown = Report {
        actions_submitted: 1,
        acknowledged_actions: 1,
        unacknowledged_actions: 1,
        submitted_operations: 2,
        disposition: InputDisposition::UnknownEffect,
        end: End::Stopped,
        ..complete
    };
    let refused = Report {
        actions_encoded: 0,
        actions_submitted: 0,
        acknowledged_actions: 0,
        submitted_operations: 0,
        stage: None,
        disposition: InputDisposition::Refused,
        host_outcome: None,
        end: End::Refused,
        ..complete
    };
    for (report, outcome, exit) in [
        (complete, "success", 0),
        (partial, "partial_submission", 1),
        (unknown, "unknown_external_effect", 1),
        (refused, "refusal", 1),
    ] {
        for json in [false, true] {
            let output = Response::Result { report }.render(json).unwrap();
            assert_eq!(output.exit, exit);
            let envelope: serde_json::Value = serde_json::from_str(&output.text).unwrap();
            assert_eq!(envelope["schema_version"], 1);
            assert_eq!(envelope["outcome"], outcome);
            assert_eq!(
                envelope["stage"],
                if report.stage.is_some() {
                    serde_json::json!("submitted_to_os")
                } else {
                    serde_json::Value::Null
                }
            );
            assert!(envelope["error"].is_null());
            let decoded: Report = serde_json::from_value(envelope["data"].clone()).unwrap();
            assert_eq!(decoded, report);
            assert_eq!(decoded.validate(), Ok(()));
            assert!(!output.text.contains("exactly_once"));
        }
    }
}

#[test]
fn malformed_reports_never_render_as_success_or_hide_unresolved_actions() {
    let complete = complete_report();
    for report in [
        Report {
            actions_total: 0,
            ..complete
        },
        Report {
            request_id: 0,
            ..complete
        },
        Report {
            acknowledged_actions: 0,
            ..complete
        },
        Report {
            unacknowledged_actions: 1,
            ..complete
        },
        Report {
            submitted_operations: 0,
            ..complete
        },
        Report {
            host_reason: Some(28),
            ..complete
        },
        Report {
            stage: Some(AcknowledgementStage::Admitted),
            ..complete
        },
        Report {
            disposition: InputDisposition::UnknownEffect,
            ..complete
        },
    ] {
        let failure = Response::Result { report }
            .render(true)
            .err()
            .expect("invalid report must refuse rendering");
        assert_eq!(failure.code, "local_session_protocol");
        assert_eq!(failure.exit, 1);
    }
}

#[test]
fn typed_refusal_codes_remain_machine_readable_and_content_free() {
    for reason in [
        Refusal::Unavailable,
        Refusal::InvalidRequest,
        Refusal::LeaseMismatch,
        Refusal::ObservationUnknown,
        Refusal::ObservationExpired,
        Refusal::ViewChanged,
        Refusal::Busy,
        Refusal::RequestChanged,
        Refusal::Consumed,
        Refusal::InputUnavailable,
        Refusal::Stopped,
    ] {
        let output = Response::Refused { reason }.render(true).unwrap();
        assert_eq!(output.exit, 1);
        let value: serde_json::Value = serde_json::from_str(&output.text).unwrap();
        assert_eq!(value["outcome"], "refusal");
        assert_eq!(value["error"]["code"], reason.code());
        assert!(value["stage"].is_null() && value["data"].is_null());
        assert!(!output.text.contains(LEASE) && !output.text.contains(CONNECTION));
    }
}

#[test]
fn modifier_chords_are_canonical_bounded_physical_key_sets() {
    let canonical = command(&action("--chord", "224,225,227,231,19"))
        .unwrap()
        .request
        .unwrap();
    let reordered = command(&action("--chord", "231,227,224,225,19"))
        .unwrap()
        .request
        .unwrap();
    assert!(canonical == reordered);
    assert!(
        canonical.operation
            == Operation::KeyChord {
                modifiers: 0b1000_1011,
                usage: 19
            }
    );
    assert_eq!(
        canonical.operation.count(),
        fr_client::robot::input::native::MAX_ACTIONS
    );
    let wire = serde_json::to_vec(&canonical).unwrap();
    assert!(wire.len() <= 512);
    assert_eq!(wire, serde_json::to_vec(&reordered).unwrap());
    assert!(serde_json::from_slice::<Request>(&wire).unwrap() == canonical);
    for (text, count) in [("224,4", 4), ("231,164", 4), ("224,228,19", 6)] {
        let parsed = command(&action("--chord", text)).unwrap().request.unwrap();
        assert!(parsed.valid());
        assert_eq!(parsed.operation.count(), count);
    }
}

#[test]
fn chords_refuse_duplicates_nonmodifiers_missing_keys_and_unbounded_sequences() {
    for text in [
        "4",
        "224",
        "224,225",
        "224,224,4",
        "231,224,231,4",
        "4,224",
        "223,4",
        "232,4",
        "-1,4",
        "256,4",
        "65536,4",
        "224,0",
        "224,3",
        "224,165",
        "224,223",
        "224,232",
        "224,-1",
        "224,65536",
        "224,225,226,227,228,4",
        "224,,4",
        ",224,4",
        "224,4,",
        "224,private-key",
    ] {
        let failure = refused(&action("--chord", text));
        assert!(!format!("{failure:?}").contains("private-key"));
    }
    let mut duplicate = action("--chord", "224,4");
    duplicate.extend(words("--chord 225,5"));
    refused(&duplicate);
}

#[test]
fn raw_chord_requests_cannot_bypass_modifier_and_key_bounds() {
    let request = command(&action("--chord", "224,4"))
        .unwrap()
        .request
        .unwrap();
    for modifiers in 0_u8..=255 {
        let mut wire = serde_json::to_value(request).unwrap();
        wire["operation"]["modifiers"] = modifiers.into();
        let decoded: Request = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded.valid(), (1..=4).contains(&modifiers.count_ones()));
        if decoded.valid() {
            assert!(decoded.operation.count() <= fr_client::robot::input::native::MAX_ACTIONS);
        }
    }
    for usage in [0, 165, 223, 224, 231, 232, 65535] {
        let mut wire = serde_json::to_value(request).unwrap();
        wire["operation"]["usage"] = usage.into();
        assert!(!serde_json::from_value::<Request>(wire).unwrap().valid());
    }
    let mut wire = serde_json::to_value(request).unwrap();
    wire["operation"]["modifiers"] = 256.into();
    assert!(serde_json::from_value::<Request>(wire).is_err());
    let mut wire = serde_json::to_value(request).unwrap();
    wire["operation"]["repeat"] = 10.into();
    assert!(serde_json::from_value::<Request>(wire).is_err());
}
