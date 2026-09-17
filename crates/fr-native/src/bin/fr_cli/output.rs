use super::Failure;
use std::{
    fmt::Write as _,
    io::{self, Write},
    time::{SystemTime, UNIX_EPOCH},
};

/// Only bounded local command output uses this encoder. No protocol decoder,
/// serializer dependency, screen, clipboard, certificate or input text is added.
pub fn quoted(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c <= '\u{1f}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
pub fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .map_or_else(|| "null".into(), |n| n.to_string())
}
pub fn failure(error: Failure, json: bool) -> String {
    if json {
        format!(
            "{{\"schema_version\":1,\"timestamp_unix_ms\":{},\"outcome\":{},\"error\":{{\"code\":{},\"next_action\":{}}}}}\n",
            timestamp(),
            quoted(if error.exit == 130 {
                "cancelled"
            } else {
                "refused"
            }),
            quoted(error.code),
            quoted(error.next)
        )
    } else {
        format!("{}: {}\n", error.code, error.next)
    }
}
pub fn write(text: &str) -> io::Result<()> {
    // The producer has finished; slow stdout never runs inside session callbacks.
    if text.len() > 2 * 1024 * 1024 {
        return Err(io::Error::other("output bound"));
    }
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()
}
#[cfg(target_os = "linux")]
pub fn hosts(snapshot: &frd::native_connection::Discovery, json: bool) -> String {
    let excluded = snapshot.excluded();
    let mut rows = String::new();
    for (i, peer) in snapshot.peers().iter().enumerate() {
        if json {
            if i != 0 {
                rows.push(',');
            }
            let ips = peer
                .addresses()
                .iter()
                .map(|a| quoted(&a.to_string()))
                .collect::<Vec<_>>()
                .join(",");
            let _ = write!(
                rows,
                "{{\"node_id\":{},\"certificate_name\":{},\"addresses\":[{}],\"desktop_available\":null,\"access_authorized\":null}}",
                quoted(peer.stable_id()),
                quoted(peer.certificate_name()),
                ips
            );
        } else {
            // Escaped text prevents local terminal controls/bidi in opaque IDs.
            let _ = writeln!(
                rows,
                "{}  {}  desktop: not probed",
                peer.stable_id().escape_default(),
                peer.certificate_name().escape_default()
            );
        }
    }
    if json {
        format!(
            "{{\"schema_version\":1,\"timestamp_unix_ms\":{},\"outcome\":\"success\",\"snapshot_only\":true,\"peers\":[{}],\"excluded\":{{\"shared\":{},\"expired\":{},\"unusable\":{}}}}}\n",
            timestamp(),
            rows,
            excluded.shared,
            excluded.expired,
            excluded.unusable
        )
    } else {
        format!(
            "Machines from installed Tailscale (desktop availability and access are unknown):\n{}Excluded: {} shared, {} expired, {} unusable.\n",
            rows, excluded.shared, excluded.expired, excluded.unusable
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_escaping_preserves_unicode_and_escapes_every_control_and_delimiter() {
        assert_eq!(
            quoted("a\"\\\n\t\0é"),
            "\"a\\\"\\\\\\u000a\\u0009\\u0000é\""
        );
        for c in '\0'..='\u{1f}' {
            assert!(!quoted(&c.to_string()).contains(c));
        }
    }
    #[test]
    fn failures_are_versioned_and_never_echo_untrusted_arguments() {
        let s = failure(
            Failure::new("cancelled", "Inspect cleanup before starting again.", 130),
            true,
        );
        assert!(s.contains("\"schema_version\":1"));
        assert!(s.contains("\"outcome\":\"cancelled\""));
        assert!(s.ends_with('\n'));
    }
}
