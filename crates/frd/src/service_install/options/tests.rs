use super::*;
use crate::service_install::{preflight_check, render_launchd_plist, render_systemd_unit};

fn parse(args: &[&str]) -> Result<InstallOptions, ServiceError> {
    InstallOptions::parse_cli(&args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
}
#[test]
fn omitted_overrides_inherit_saved_policy() {
    let options = parse(&["--software-explicit", "--dry-run"]).unwrap();
    assert_eq!(options.approval_mode, "");
    assert_eq!(options.sharing_scope, "");
    let unit = render_systemd_unit(&options);
    assert!(!unit.contains("--approval"));
    assert!(!unit.contains("--sharing"));
}
#[test]
fn explicit_default_values_are_not_dropped() {
    let options = parse(&[
        "--approval",
        "none",
        "--sharing",
        "own-user",
        "--software-explicit",
        "--dry-run",
    ])
    .unwrap();
    let unit = render_systemd_unit(&options);
    assert!(unit.contains("--approval none --sharing own-user"));
    let plist = render_launchd_plist(&options);
    assert!(plist.contains("<string>--approval</string>\n        <string>none</string>"));
    assert!(plist.contains("<string>--sharing</string>\n        <string>own-user</string>"));
    assert_eq!(
        parse(&["--approval", "unattended", "--software-explicit"])
            .unwrap()
            .approval_mode,
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
        software_explicit: true,
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
    // The writer lock is a nonblocking flock. A child forked by a concurrent
    // test thread briefly inherits the lock's open file description until its
    // exec (CLOEXEC), so the next update can see a transient Busy. This test
    // is about path resolution, not contention: retry Busy, bounded.
    let update = |change: Change| {
        for _ in 0..200 {
            match store.update(change) {
                Err(crate::host_policy::Error::Busy) => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                other => return other.unwrap(),
            }
        }
        panic!("policy store stayed Busy for a second");
    };
    update(Change::Approval(Approval::Local));
    update(Change::Sharing(Sharing::Tailnet));
    for explicit in [false, true] {
        let mut argv = vec![
            "--config",
            config.to_str().unwrap(),
            "--software-explicit",
            "--dry-run",
        ];
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
#[test]
#[cfg(target_os = "linux")]
fn systemd_install_refuses_units_that_frd_run_would_refuse() {
    // Without the explicit software profile, frd run exits at every start and
    // Restart=always would loop forever.
    assert_eq!(
        parse(&["--dry-run"]).err(),
        Some(ServiceError::HostProfileUnavailable {
            code: "hardware_hevc_unavailable",
            detail: "frd run has no hardware HEVC encoder selection yet; pass --software-explicit to install the CPU software profile",
        })
    );
    let local = parse(&["--software-explicit", "--approval", "local", "--dry-run"]).err();
    assert!(
        matches!(
            local,
            Some(ServiceError::HostProfileUnavailable {
                code: "local_approval_unavailable",
                ..
            })
        ),
        "{local:?}"
    );
    let options = parse(&["--software-explicit", "--approval", "none", "--dry-run"]).unwrap();
    let unit = render_systemd_unit(&options);
    assert!(
        unit.contains(" --approval none --software-explicit\n"),
        "{unit}"
    );
}

#[test]
fn frd_run_options_after_double_dash_reach_the_unit_validated_by_frd_run() {
    let options = parse(&[
        "--user",
        "--software-explicit",
        "--approval",
        "none",
        "--",
        "--input-agent",
        "/usr/libexec/fr-input-agent",
        "--clipboard",
        "--logind-session",
        "c2",
    ])
    .unwrap();
    assert_eq!(
        options.run_args,
        [
            "--input-agent",
            "/usr/libexec/fr-input-agent",
            "--clipboard",
            "--logind-session",
            "c2"
        ]
    );
    let unit = crate::service_install::render_systemd_unit(&options);
    assert!(
        unit.contains(
            "--software-explicit --input-agent /usr/libexec/fr-input-agent --clipboard --logind-session c2"
        ),
        "{unit}"
    );
    assert!(unit.contains("WantedBy=graphical-session.target"), "{unit}");
    assert!(unit.contains("RestartPreventExitStatus=2"), "{unit}");
}

#[test]
fn frd_run_options_that_every_start_would_refuse_are_refused_at_install() {
    let base = ["--user", "--software-explicit", "--approval", "none", "--"];
    for (tail, code) in [
        (&["--clipboard"][..], Some("clipboard_requires_input_agent")),
        (
            &["--files", "/srv/drop"][..],
            Some("files_requires_input_agent"),
        ),
        // frd run's own parser refuses these.
        (&["--no-such-flag"][..], None),
        (&["--input-agent"][..], None),
        // Set by the installer's own flags, or meaningless for a service.
        (&["--port", "9000"][..], None),
        (&["--approval", "none"][..], None),
        (&["--once"][..], None),
        // Paths in a unit must be absolute.
        (&["--input-agent", "relative/fr-input-agent"][..], None),
    ] {
        let args: Vec<&str> = base.iter().chain(tail).copied().collect();
        match (parse(&args), code) {
            (Err(ServiceError::HostProfileUnavailable { code: got, .. }), Some(want)) => {
                assert_eq!(got, want, "{tail:?}");
            }
            (Err(ServiceError::InvalidOptions), None) => {}
            (other, _) => panic!("{tail:?}: {other:?}"),
        }
    }
    // A system unit has no user's X display: only a private headless one.
    let refused = parse(&[
        "--system",
        "--software-explicit",
        "--approval",
        "none",
        "--",
        "--input-agent",
        "/usr/libexec/fr-input-agent",
    ]);
    assert!(matches!(
        refused,
        Err(ServiceError::HostProfileUnavailable {
            code: "system_service_requires_headless",
            ..
        })
    ));
    assert!(
        parse(&[
            "--system",
            "--software-explicit",
            "--approval",
            "none",
            "--",
            "--headless"
        ])
        .is_ok()
    );
}
