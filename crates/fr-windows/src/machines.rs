//! Presentation rows for the native machine picker: labels, ordering and
//! search over one installed-Tailscale snapshot.
//!
//! Everything here is display metadata. A row's operating system and online
//! state come from the same snapshot as its identity, but neither says that
//! `FrankenRemote` is running there, that a desktop exists, or that this
//! viewer will be admitted. Selection still goes through the original catalog
//! index and the connector's fresh authenticated lookup.
#![forbid(unsafe_code)]

use fr_client::discovery::picker::PathHint;
use std::cmp::Ordering;

/// The operating system a node reports, for labels and ordering only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    Windows,
    Mac,
    Linux,
    Ios,
    Android,
    Other,
}

impl Platform {
    /// Tailscale's `OS` strings ("windows", "macOS", "linux", "iOS",
    /// "android", ...). Absent values stay absent.
    pub(crate) fn from_os(os: Option<&str>) -> Option<Self> {
        let os = os?.trim();
        if os.is_empty() {
            return None;
        }
        Some(match os.to_ascii_lowercase().as_str() {
            "windows" => Self::Windows,
            "macos" | "darwin" => Self::Mac,
            "linux" | "freebsd" | "openbsd" => Self::Linux,
            "ios" | "tvos" | "ipados" => Self::Ios,
            "android" => Self::Android,
            _ => Self::Other,
        })
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::Mac => "macOS",
            Self::Linux => "Linux",
            Self::Ios => "iOS",
            Self::Android => "Android",
            Self::Other => "Other",
        }
    }

    /// Phones and tablets run `FrankenRemote` only as a viewer.
    pub(crate) const fn hosts_desktops(self) -> bool {
        !matches!(self, Self::Ios | Self::Android)
    }
}

/// One picker row. `index` is the row's ordinal in the ORIGINAL catalog; the
/// native list carries it so sorting and filtering never change identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub index: usize,
    pub stable_id: String,
    pub full_name: String,
    pub platform: Option<Platform>,
    pub online: Option<bool>,
    pub path: PathHint,
    pub address: Option<String>,
}

impl Row {
    /// The first DNS label of the canonical name (`mac-studio` for
    /// `mac-studio.example.ts.net`).
    pub(crate) fn short_name(&self) -> &str {
        short_name(&self.full_name)
    }

    pub(crate) fn status_label(&self) -> &'static str {
        match self.online {
            Some(true) => "Online",
            Some(false) => "Offline",
            None => "Unknown",
        }
    }

    pub(crate) fn system_label(&self) -> &'static str {
        match self.platform {
            Some(Platform::Ios) => "iOS (viewer only)",
            Some(Platform::Android) => "Android (viewer only)",
            Some(platform) => platform.label(),
            None => "",
        }
    }

    pub(crate) const fn path_label(&self) -> &'static str {
        match self.path {
            PathHint::Direct => "Direct",
            PathHint::Relayed => "Relayed",
            PathHint::Unknown => "Idle",
        }
    }

    /// Dimmed in the list: offline or unable to host.
    pub(crate) fn dimmed(&self) -> bool {
        self.online == Some(false) || !self.platform.is_none_or(Platform::hosts_desktops)
    }

    /// Case-insensitive substring match over every visible column.
    pub(crate) fn matches(&self, query: &str) -> bool {
        let query = query.trim();
        if query.is_empty() {
            return true;
        }
        let query = query.to_lowercase();
        [
            self.full_name.as_str(),
            self.system_label(),
            self.status_label(),
            self.path_label(),
            self.address.as_deref().unwrap_or(""),
        ]
        .iter()
        .any(|field| field.to_lowercase().contains(&query))
    }

    /// Group order: hostable and online, hostable and unknown, hostable and
    /// offline, then viewer-only devices.
    fn group(&self) -> u8 {
        let hostable = self.platform.is_none_or(Platform::hosts_desktops);
        match (hostable, self.online) {
            (true, Some(true)) => 0,
            (true, None) => 1,
            (true, Some(false)) => 2,
            (false, _) => 3,
        }
    }
}

pub(crate) fn short_name(full: &str) -> &str {
    let full = full.trim_end_matches('.');
    full.split('.')
        .next()
        .filter(|label| !label.is_empty())
        .unwrap_or(full)
}

/// Stable display order: group, then short name (case-insensitive), then the
/// full canonical name so equal short names never swap between refreshes.
pub(crate) fn sort(rows: &mut [Row]) {
    rows.sort_by(|a, b| {
        a.group()
            .cmp(&b.group())
            .then_with(|| compare_names(a.short_name(), b.short_name()))
            .then_with(|| a.full_name.cmp(&b.full_name))
    });
}

fn compare_names(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

/// Counts for the status line: (listed, online, hostable).
pub(crate) fn counts(rows: &[Row]) -> (usize, usize, usize) {
    (
        rows.len(),
        rows.iter().filter(|row| row.online == Some(true)).count(),
        rows.iter()
            .filter(|row| row.platform.is_none_or(Platform::hosts_desktops))
            .count(),
    )
}

/// The first address to show: IPv4 when present (shorter), else the first.
pub(crate) fn display_address(addresses: &[std::net::IpAddr]) -> Option<String> {
    addresses
        .iter()
        .find(|address| address.is_ipv4())
        .or_else(|| addresses.first())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(index: usize, name: &str, os: Option<&str>, online: Option<bool>) -> Row {
        Row {
            index,
            stable_id: format!("n{index}"),
            full_name: format!("{name}.tail.ts.net"),
            platform: Platform::from_os(os),
            online,
            path: PathHint::Unknown,
            address: Some(format!("100.64.0.{index}")),
        }
    }

    #[test]
    fn tailscale_os_strings_map_to_platforms_and_hosting_ability() {
        for (os, platform, hosts) in [
            ("windows", Platform::Windows, true),
            ("macOS", Platform::Mac, true),
            ("linux", Platform::Linux, true),
            ("iOS", Platform::Ios, false),
            ("android", Platform::Android, false),
            ("plan9", Platform::Other, true),
        ] {
            let mapped = Platform::from_os(Some(os)).unwrap();
            assert_eq!(mapped, platform);
            assert_eq!(mapped.hosts_desktops(), hosts);
        }
        assert_eq!(Platform::from_os(None), None);
        assert_eq!(Platform::from_os(Some("  ")), None);
    }

    #[test]
    fn rows_sort_hostable_online_first_then_by_name_and_stay_stable() {
        let mut rows = vec![
            row(0, "zeta", Some("linux"), Some(true)),
            row(1, "phone", Some("iOS"), Some(true)),
            row(2, "Alpha", Some("windows"), Some(false)),
            row(3, "beta", Some("macOS"), Some(true)),
            row(4, "gamma", None, None),
            row(5, "alpha", Some("linux"), Some(false)),
        ];
        sort(&mut rows);
        let order: Vec<usize> = rows.iter().map(|row| row.index).collect();
        // Online hostable (beta, zeta), unknown (gamma), offline (Alpha and
        // alpha by full name), then the viewer-only phone.
        assert_eq!(order, [3, 0, 4, 2, 5, 1]);
        let mut again = rows.clone();
        again.reverse();
        sort(&mut again);
        assert_eq!(again, rows);
    }

    #[test]
    fn search_matches_any_visible_column_case_insensitively() {
        let mut target = row(7, "Mac-Studio", Some("macOS"), Some(true));
        target.path = PathHint::Direct;
        for query in [
            "",
            "  ",
            "studio",
            "MAC-STUDIO.tail",
            "macos",
            "online",
            "direct",
            "100.64.0.7",
        ] {
            assert!(target.matches(query), "{query}");
        }
        for query in ["windows", "offline", "relayed", "100.64.0.8"] {
            assert!(!target.matches(query), "{query}");
        }
    }

    #[test]
    fn labels_and_dimming_describe_only_the_snapshot() {
        let phone = row(1, "phone", Some("android"), Some(true));
        assert!(phone.dimmed());
        assert_eq!(phone.system_label(), "Android (viewer only)");
        let offline = row(2, "box", Some("linux"), Some(false));
        assert!(offline.dimmed());
        assert_eq!(offline.status_label(), "Offline");
        let unknown = row(3, "box2", None, None);
        assert!(!unknown.dimmed());
        assert_eq!(
            (unknown.status_label(), unknown.system_label()),
            ("Unknown", "")
        );
        assert_eq!(unknown.path_label(), "Idle");
        assert_eq!(short_name("host.example.ts.net."), "host");
        assert_eq!(short_name("single"), "single");
        assert_eq!(counts(&[phone, offline, unknown]), (3, 1, 2));
    }

    #[test]
    fn the_displayed_address_prefers_ipv4() {
        let v6: std::net::IpAddr = "fd7a:115c:a1e0::1".parse().unwrap();
        let v4: std::net::IpAddr = "100.64.0.1".parse().unwrap();
        assert_eq!(display_address(&[v6, v4]).as_deref(), Some("100.64.0.1"));
        assert_eq!(display_address(&[v6]).as_deref(), Some("fd7a:115c:a1e0::1"));
        assert_eq!(display_address(&[]), None);
    }
}
