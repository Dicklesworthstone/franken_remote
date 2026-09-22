use super::*;
use crate::service_install::{preflight_check, render_launchd_plist, render_systemd_unit};

fn parse(args: &[&str]) -> Result<InstallOptions, ServiceError> {
    InstallOptions::parse_cli(&args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
}
#[test]
fn omitted_overrides_inherit_saved_policy() {
    let options = parse(&["--dry-run"]).unwrap();
    assert_eq!(options.approval_mode, "");
    assert_eq!(options.sharing_scope, "");
    let unit = render_systemd_unit(&options);
    assert!(!unit.contains("--approval"));
    assert!(!unit.contains("--sharing"));
}
#[test]
fn explicit_default_values_are_not_dropped() {
    let options = parse(&["--approval", "none", "--sharing", "own-user", "--dry-run"]).unwrap();
    let unit = render_systemd_unit(&options);
    assert!(unit.contains("--approval none --sharing own-user"));
    let plist = render_launchd_plist(&options);
    assert!(plist.contains("<string>--approval</string>\n        <string>none</string>"));
    assert!(plist.contains("<string>--sharing</string>\n        <string>own-user</string>"));
    assert_eq!(
        parse(&["--approval", "unattended"]).unwrap().approval_mode,
        "none"
    );
}
#[test]
fn invalid_options_refuse_even_for_dry_run() {
    for args in [
        vec!["--approval", "loacl"],
        vec!["--sharing", "everyone"],
        vec!["--port", "0"],
        vec!["--port", "65536"],
        vec!["--port", "8443", "--port", "9443"],
        vec!["--approval", "local", "--approval", "none"],
        vec!["--user", "--system"],
        vec!["--user", "--user"],
        vec!["--config"],
        vec!["--config", "--json"],
        vec!["--socket", "relative.sock"],
        vec!["--config", "/tmp/../etc/policy.json"],
        vec!["--config", "/tmp/host\nExecStart=/evil"],
        vec!["--unknown"],
        vec!["--json", "--json"],
        vec!["--dry-run", "--dry-run"],
        vec!["ignored"],
    ] {
        let mut argv: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        argv.push("--dry-run".into());
        assert!(InstallOptions::parse_cli(&argv).is_err(), "{args:?}");
    }
}
#[test]
fn programmatic_callers_cannot_bypass_validation() {
    let mut options = InstallOptions {
        dry_run: true,
        ..InstallOptions::default()
    };
    options.sharing_scope = "tailnet\nExecStart=/evil".into();
    assert_eq!(preflight_check(&options), Err(ServiceError::InvalidOptions));
    options.sharing_scope.clear();
    options.service_port = 0;
    assert_eq!(preflight_check(&options), Err(ServiceError::InvalidOptions));
}
#[test]
fn systemd_paths_remain_single_literal_arguments() {
    let options = InstallOptions {
        kind: ServiceKind::SystemdUser,
        exec_path: PathBuf::from("/opt/frd \"special\"/$HOST%name"),
        socket_path: Some(PathBuf::from("/run/a & b/$SOCKET%1.sock")),
        config_path: Some(PathBuf::from("/etc/frd/policy with \\ and % and $.json")),
        dry_run: true,
        ..InstallOptions::default()
    };
    options.validate().unwrap();
    let unit = render_systemd_unit(&options);
    assert!(unit.contains("ExecStart=:\"/opt/frd \\\"special\\\"/$HOST%%name\" run --port 8443"));
    assert!(unit.contains("--socket \"/run/a & b/$SOCKET%%1.sock\""));
    assert!(unit.contains("--config \"/etc/frd/policy with \\\\ and %% and $.json\""));
    assert_eq!(
        unit.lines()
            .filter(|line| line.starts_with("ExecStart="))
            .count(),
        1
    );
}
#[test]
fn launchd_paths_are_xml_text_not_markup() {
    let options = InstallOptions {
        kind: ServiceKind::LaunchdAgent,
        exec_path: PathBuf::from("/opt/a&b<frd>\"'"),
        socket_path: Some(PathBuf::from("/run/a&b<socket>")),
        dry_run: true,
        ..InstallOptions::default()
    };
    let plist = render_launchd_plist(&options);
    assert!(plist.contains("<string>/opt/a&amp;b&lt;frd&gt;&quot;&apos;</string>"));
    assert!(plist.contains("<string>/run/a&amp;b&lt;socket&gt;</string>"));
    assert!(!plist.contains("<socket>"));
}
#[test]
fn unavailable_platform_overrides_refuse_instead_of_disappearing() {
    let mut options = InstallOptions {
        kind: ServiceKind::LaunchdAgent,
        config_path: Some(PathBuf::from("/etc/frd/policy.json")),
        ..InstallOptions::default()
    };
    assert!(matches!(
        options.validate(),
        Err(ServiceError::UnsupportedPlatform { .. })
    ));
    options.config_path = None;
    options.kind = ServiceKind::WindowsService;
    options.approval_mode = "local".into();
    assert!(matches!(
        options.validate(),
        Err(ServiceError::UnsupportedPlatform { .. })
    ));
}
#[test]
#[cfg(target_os = "linux")]
fn selected_policy_path_and_explicit_overrides_reach_startup_resolution() {
    use crate::host_policy::{Approval, Change, Sharing, Store, options::RunOptions};
    use std::{
        fs,
        os::unix::fs::DirBuilderExt,
        time::{SystemTime, UNIX_EPOCH},
    };
    struct Root(PathBuf);
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let root = Root(std::env::temp_dir().join(format!(
            "frd-install-policy-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    fs::DirBuilder::new().mode(0o700).create(&root.0).unwrap();
    let config = root.0.join("policy.json");
    let store = Store::new(&config).unwrap();
    store.update(Change::Approval(Approval::Local)).unwrap();
    store.update(Change::Sharing(Sharing::Tailnet)).unwrap();
    for explicit in [false, true] {
        let mut argv = vec!["--config", config.to_str().unwrap(), "--dry-run"];
        if explicit {
            argv.extend(["--approval", "none", "--sharing", "own-user"]);
        }
        let options = parse(&argv).unwrap();
        let unit = render_systemd_unit(&options);
        // All paths in this fixture are ASCII without whitespace; escaping has
        // separate regressions. Feed the emitted run argv to the real parser.
        let command = unit
            .lines()
            .find_map(|l| l.strip_prefix("ExecStart=:"))
            .unwrap();
        let run_argv: Vec<String> = command
            .split_whitespace()
            .skip(2)
            .map(str::to_owned)
            .collect();
        let effective = RunOptions::parse(&run_argv).unwrap().resolve().unwrap();
        assert_eq!(
            effective.approval,
            if explicit {
                Approval::None
            } else {
                Approval::Local
            }
        );
        assert_eq!(
            effective.sharing,
            if explicit {
                Sharing::OwnUser
            } else {
                Sharing::Tailnet
            }
        );
        assert_eq!(effective.saved.revision, 2);
    }
    assert_eq!(store.load().unwrap().revision, 2);
}
