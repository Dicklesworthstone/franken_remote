use super::{INPUT, InvalidRequest, OBSERVE, OVERVIEW, ROBOT, SEND, page};

fn request(args: &[&str]) -> Result<Option<&'static str>, InvalidRequest> {
    page(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
}

#[test]
fn every_topic_has_bounded_help_in_both_command_forms() {
    for (topic, expected) in [
        (&["connect"][..], OVERVIEW),
        (&["hosts"][..], OVERVIEW),
        (&["hosts", "save"][..], OVERVIEW),
        (&["hosts", "saved"][..], OVERVIEW),
        (&["hosts", "forget"][..], OVERVIEW),
        (&["status"][..], OVERVIEW),
        (&["doctor"][..], OVERVIEW),
        (&["inspect"][..], OVERVIEW),
        (&["displays"][..], OVERVIEW),
        (&["disconnect"][..], OVERVIEW),
        (&["robot"][..], ROBOT),
        (&["robot", "session"][..], OVERVIEW),
        (&["robot", "session", "open"][..], OVERVIEW),
        (&["robot", "session", "status"][..], OVERVIEW),
        (&["robot", "session", "close"][..], OVERVIEW),
        (&["robot", "observe"][..], OBSERVE),
        (&["robot", "input"][..], INPUT),
        (&["send"][..], SEND),
    ] {
        assert!(expected.len() < 16 * 1024, "help page exceeds 16 KiB");
        let mut prefix = vec!["help"];
        prefix.extend_from_slice(topic);
        assert_eq!(request(&prefix), Ok(Some(expected)));
        let mut suffix = topic.to_vec();
        suffix.push("--help");
        assert_eq!(request(&suffix), Ok(Some(expected)));
    }
    for args in [&[][..], &["--help"][..], &["help"][..], &["help", "--help"]] {
        assert_eq!(request(args), Ok(Some(OVERVIEW)));
    }
}

#[test]
fn help_does_not_hide_unknown_topics_targets_or_options() {
    for args in [
        &["help", "unknown"][..],
        &["help", "robot", "unknown"][..],
        &["help", "robot", "session", "unknown"][..],
        &["help", "robot", "input", "private-target"][..],
        &["help", "send", "--json"][..],
        &["robot", "observe", "private-target", "--help"][..],
        &["connect", "private-target", "--view-only", "--help"][..],
        &["connect", "--help", "--json"][..],
        &["--help", "--help"][..],
        &["help", "robot\ninput"][..],
    ] {
        assert_eq!(request(args), Err(InvalidRequest));
    }
}

#[test]
fn ordinary_commands_keep_their_original_parser() {
    for args in [
        &["hosts"][..],
        &["robot", "session", "status", "help", "--json"][..],
        &["send", "private-target", "--connection-id", "private-id"][..],
    ] {
        assert_eq!(request(args), Ok(None));
    }
}
