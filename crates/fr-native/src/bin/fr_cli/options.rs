use super::Failure;
use std::{collections::BTreeSet, path::PathBuf};

pub struct Options {
    pub command: Command,
    pub json: bool,
    pub socket: Option<PathBuf>,
}
pub enum Command {
    Help,
    Hosts,
    Connect(Connection),
}
pub struct Connection {
    pub node: String,
    pub by_name: bool,
    pub display: u128,
    pub worker: PathBuf,
    pub roots: PathBuf,
    pub x_display: Option<String>,
    pub port: u16,
    pub ipv6: bool,
    pub attempts: u8,
}
fn usage() -> Failure {
    Failure::new(
        "invalid_arguments",
        "Run fr --help; arguments are never echoed.",
        2,
    )
}
fn path(value: String) -> Result<PathBuf, Failure> {
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(usage());
    }
    Ok(path)
}
/// Bounded argument grammar, no shell, URL, credential, arbitrary-command or
/// remote-policy option. `connect` without explicit view-only refuses; it never
/// silently substitutes viewing for the product's requested control behavior.
pub fn parse(args: &[String]) -> Result<Options, Failure> {
    if args.len() > 32
        || args
            .iter()
            .any(|s| s.len() > 4096 || s.chars().any(char::is_control))
    {
        return Err(usage());
    }
    if args.is_empty() || args == ["--help"] || args == ["help"] {
        return Ok(Options {
            command: Command::Help,
            json: false,
            socket: None,
        });
    }
    let connect = match args[0].as_str() {
        "hosts" => false,
        "connect" => true,
        _ => return Err(usage()),
    };
    let mut index = 1;
    let node = if connect {
        let node = args.get(index).ok_or_else(usage)?;
        if node.is_empty()
            || node.len() > 254
            || node.starts_with('-')
            || !node.is_ascii()
            || node
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b"/:?#@\\".contains(&b))
        {
            return Err(usage());
        }
        index += 1;
        Some(node.clone())
    } else {
        None
    };
    parse_command(args, index, node)
}
fn parse_command(
    args: &[String],
    mut index: usize,
    node: Option<String>,
) -> Result<Options, Failure> {
    let connect = node.is_some();
    let mut seen = BTreeSet::new();
    let (mut json, mut socket, mut by_name, mut view_only, mut experimental, mut ipv6) =
        (false, None, false, false, false, false);
    let (mut display, mut worker, mut roots, mut x_display) = (None, None, None, None);
    let (mut port, mut attempts) = (8443_u16, 5_u8);
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        if !seen.insert(flag) {
            return Err(usage());
        }
        let value = |index: &mut usize| -> Result<String, Failure> {
            let value = args
                .get(*index)
                .filter(|s| !s.is_empty() && !s.starts_with("--"))
                .ok_or_else(usage)?
                .clone();
            *index += 1;
            Ok(value)
        };
        match flag {
            "--json" => json = true,
            "--socket" => socket = Some(path(value(&mut index)?)?),
            "--by-name" if connect => by_name = true,
            "--view-only" if connect => view_only = true,
            "--experimental-native" if connect => experimental = true,
            "--ipv6" if connect => ipv6 = true,
            "--display" if connect => {
                display = Some(value(&mut index)?.parse::<u128>().map_err(|_| usage())?);
            }
            "--worker" if connect => worker = Some(path(value(&mut index)?)?),
            "--trust-roots" if connect => roots = Some(path(value(&mut index)?)?),
            "--x-display" if connect => x_display = Some(value(&mut index)?),
            "--port" if connect => port = value(&mut index)?.parse().map_err(|_| usage())?,
            "--attempts" if connect => {
                attempts = value(&mut index)?.parse().map_err(|_| usage())?;
            }
            _ => return Err(usage()),
        }
    }
    let command = if let Some(node) = node {
        if !view_only {
            return Err(Failure::new(
                "control_ui_unavailable",
                "This client currently requires --view-only; no control is silently granted or requested.",
                2,
            ));
        }
        if !experimental {
            return Err(Failure::new(
                "native_transport_unqualified",
                "Native transport is experimental; explicitly select --experimental-native only for development/qualification.",
                2,
            ));
        }
        if (!by_name && node.len() > 128)
            || port == 0
            || !(1..=32).contains(&attempts)
            || display == Some(0)
        {
            return Err(usage());
        }
        Command::Connect(Connection {
            node,
            by_name,
            display: display.ok_or_else(usage)?,
            worker: worker.ok_or_else(usage)?,
            roots: roots.ok_or_else(usage)?,
            x_display,
            port,
            ipv6,
            attempts,
        })
    } else {
        Command::Hosts
    };
    Ok(Options {
        command,
        json,
        socket,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn options(s: &str) -> Result<Options, Failure> {
        parse(&s.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
    }
    #[test]
    fn refuses_implicit_control_or_unqualified_transport_before_any_io() {
        assert_eq!(
            options("connect n-host").err().unwrap().code,
            "control_ui_unavailable"
        );
        assert_eq!(
            options("connect n-host --view-only").err().unwrap().code,
            "native_transport_unqualified"
        );
        assert!(options("connect n-host --view-only --experimental-native").is_err());
    }
    #[test]
    fn parses_only_explicit_bounded_connection_settings() {
        let o = options("connect n-host --view-only --experimental-native --worker /opt/fr/worker --trust-roots /opt/fr/ca.pem --display 9 --ipv6 --attempts 2 --json").unwrap();
        let Command::Connect(c) = o.command else {
            panic!("connection required")
        };
        assert_eq!(c.node, "n-host");
        assert_eq!(c.display, 9);
        assert_eq!(c.attempts, 2);
        assert!(c.ipv6 && o.json);
        assert!(!c.by_name);
    }
    #[test]
    fn unknown_duplicate_credential_and_policy_switches_are_not_ignored() {
        for s in [
            "hosts --json --json",
            "hosts --socket /a --socket /b",
            "hosts --port 8443",
            "hosts --socket ../sock",
            "connect https://host.invalid",
            "hosts --token secret",
            "connect n-peer --approval none",
            "hosts garbage",
        ] {
            assert!(options(s).is_err(), "{s}");
        }
    }
    #[test]
    fn rejects_oversized_and_control_character_arguments_without_echoing_them() {
        for a in [
            vec!["hosts".into(), "x".repeat(4097)],
            vec!["hosts".into(); 33],
            vec!["hosts".into(), "private\ncontent".into()],
        ] {
            let error = parse(&a).err().unwrap();
            assert!(!format!("{error:?}").contains("private"));
        }
    }
}
