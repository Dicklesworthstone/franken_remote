//! Read-only lookup and comparison-only instance pins for the local owner.
//! Neither command opens a remote session or accepts an input-lease credential.
use super::{Command, Failure, Options, RobotCommand, usage, validate_node};

pub(super) fn status(args: &[String]) -> Result<Options, Failure> {
    let node = args.get(3).ok_or_else(usage)?;
    validate_node(node)?;
    // The local authenticated identity record has a 253-byte target ceiling.
    if node.len() > 253 {
        return Err(usage());
    }
    let json = match &args[4..] {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => return Err(usage()),
    };
    Ok(Options {
        command: Command::Robot(RobotCommand::SessionStatus { node: node.clone() }),
        json,
        socket: None,
    })
}

/// Preserve the canonical identifier exactly; never accept a prefix, numeric
/// truncation, alias or case-folded comparison. Errors do not echo the value.
pub(super) fn identifier(value: &str) -> Result<String, Failure> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(usage());
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Options, Failure> {
        super::super::parse(
            &text
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn session_status_selects_only_an_exact_local_target() {
        for suffix in ["", " --json"] {
            let options = parse(&format!("robot session status n-host{suffix}")).unwrap();
            let Command::Robot(RobotCommand::SessionStatus { node }) = options.command else {
                panic!("local status command required");
            };
            assert_eq!(node, "n-host");
            assert_eq!(options.json, !suffix.is_empty());
            assert!(options.socket.is_none());
        }
        assert!(parse(&format!("robot session status {}", "a".repeat(253))).is_ok());
        assert!(parse(&format!("robot session status {}", "a".repeat(254))).is_err());
    }

    #[test]
    fn local_status_never_ignores_remote_options_or_credentials() {
        for text in [
            "robot session status",
            "robot session status n-host --json --json",
            "robot session status n-host --socket /tmp/remote.sock",
            "robot session status n-host --port 9443",
            "robot session status n-host --role control",
            "robot session status n-host --lease private-lease",
            "robot session status n-host --connection-id 0123456789abcdef0123456789abcdef",
            "robot session status n-host unexpected",
        ] {
            let failure = parse(text).err().expect("invalid local status");
            assert_eq!(failure.code, "invalid_arguments");
            assert!(!format!("{failure:?}").contains("private-lease"));
        }
    }

    #[test]
    fn conditional_close_preserves_the_complete_comparison_identifier() {
        let id = "0123456789abcdef0123456789abcdef";
        let options = parse(&format!(
            "robot session close n-host --connection-id {id} --json"
        ))
        .unwrap();
        let Command::Robot(RobotCommand::SessionClose(close)) = options.command else {
            panic!("local close command required");
        };
        assert_eq!(close.node, "n-host");
        assert_eq!(close.connection_id.as_deref(), Some(id));
        assert!(close.lease.is_none());
        assert!(options.json);
    }

    #[test]
    fn malformed_or_duplicate_instance_pins_are_rejected_before_io() {
        for id in [
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcdeg",
            "*",
            "private-instance",
        ] {
            let failure = parse(&format!("robot session close n-host --connection-id {id}"))
                .err()
                .expect("invalid comparison identifier");
            assert_eq!(failure.code, "invalid_arguments");
            assert!(!format!("{failure:?}").contains("private-instance"));
        }
        for flags in [
            "--connection-id",
            "--connection-id --json",
            "--connection-id 0123456789abcdef0123456789abcdef --connection-id 0123456789abcdef0123456789abcdef",
        ] {
            assert!(parse(&format!("robot session close n-host {flags}")).is_err());
        }
    }

    #[test]
    fn legacy_close_and_unissued_lease_selection_remain_distinct() {
        for suffix in ["", " --lease lease-123"] {
            let options = parse(&format!("robot session close n-host{suffix}")).unwrap();
            let Command::Robot(RobotCommand::SessionClose(close)) = options.command else {
                panic!("local close command required");
            };
            assert!(close.connection_id.is_none());
            assert_eq!(
                close.lease.as_deref(),
                (!suffix.is_empty()).then_some("lease-123")
            );
        }
        // Parsing never silently turns a remote lease into a local instance pin.
        let options = parse(
            "robot session close n-host --lease lease-123 --connection-id 0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let Command::Robot(RobotCommand::SessionClose(close)) = options.command else {
            panic!("local close command required");
        };
        assert_eq!(close.lease.as_deref(), Some("lease-123"));
        assert!(close.connection_id.is_some());
    }
}
