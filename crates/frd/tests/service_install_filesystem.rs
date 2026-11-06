#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]
//! Real filesystem exercises of the public installer, independent of any
//! running service manager, Tailscale peer, or simulated filesystem.
use frd::service_install::{
    INGRESS_HELPER_EXEC, InstallOptions, ServiceError, ServiceKind, install, render_ingress_helper,
    render_systemd_unit, resolve_unit_path, uninstall,
};
use std::{
    fs,
    net::TcpListener,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        let path =
            std::env::temp_dir().join(format!("frd-install-{:032x}", u128::from_ne_bytes(random)));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }

    fn options(&self) -> InstallOptions {
        let socket = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        InstallOptions {
            kind: ServiceKind::SystemdUser,
            service_port: socket.local_addr().unwrap().port(),
            exec_path: std::env::current_exe().unwrap(),
            custom_unit_dir: Some(self.0.join("units")),
            software_explicit: true,
            ..InstallOptions::default()
        }
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        // Only this test's freshly created random directory is owned here.
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn unit_directory(options: &InstallOptions) -> &Path {
    options.custom_unit_dir.as_deref().unwrap()
}

fn create_unit_directory(options: &InstallOptions) {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(unit_directory(options))
        .unwrap();
}

fn no_private_stages(options: &InstallOptions) {
    for item in fs::read_dir(unit_directory(options)).unwrap() {
        assert!(
            !item
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".frd-unit-pending-"),
            "an owned service stage was not retired"
        );
    }
}

#[test]
fn preview_performs_no_filesystem_publication() {
    let root = Root::new();
    let mut options = root.options();
    options.dry_run = true;
    let report = install(&options).unwrap();
    assert_eq!(report.durable, None);
    assert_eq!(report.unit_content, render_systemd_unit(&options));
    assert!(!unit_directory(&options).exists());
}

#[test]
fn root_helper_emits_the_verified_executable_instead_of_a_mutable_alias() {
    let root = Root::new();
    let mut options = root.options();
    let protected = PathBuf::from("/usr/bin/env").canonicalize().unwrap();
    let alias = root.0.join("mutable-helper-alias");
    symlink(&protected, &alias).unwrap();
    options.exec_path = alias.clone();

    let helper = render_ingress_helper(&options, 1000);
    assert_eq!(helper.exec_path, protected);
    assert_eq!(helper.copy_from, None);
    assert!(!helper.unit_content.contains(alias.to_str().unwrap()));
    assert!(helper.unit_content.contains(&format!(
        "ExecStart=:{} ingress-helper",
        protected.display()
    )));

    let replacement = root.0.join("user-writable-executable");
    fs::write(&replacement, b"#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();
    let next_alias = root.0.join("replacement-alias");
    symlink(&replacement, &next_alias).unwrap();
    fs::rename(&next_alias, &alias).unwrap();
    assert_eq!(alias.canonicalize().unwrap(), replacement);
    assert_eq!(helper.exec_path, protected);
    assert!(!helper.unit_content.contains(alias.to_str().unwrap()));

    let untrusted = render_ingress_helper(&options, 1000);
    assert_eq!(untrusted.exec_path, PathBuf::from(INGRESS_HELPER_EXEC));
    assert_eq!(untrusted.copy_from, Some(alias));
}

#[test]
fn a_root_owned_directory_or_nonexecutable_file_is_not_a_helper_executable() {
    for path in ["/usr/bin", "/etc/hosts"] {
        let root = Root::new();
        let options = InstallOptions {
            exec_path: PathBuf::from(path),
            ..root.options()
        };
        let helper = render_ingress_helper(&options, 1000);
        assert_eq!(helper.exec_path, PathBuf::from(INGRESS_HELPER_EXEC));
        assert_eq!(helper.copy_from, Some(PathBuf::from(path)));
    }
}

#[test]
fn repeated_install_publishes_whole_durable_regular_units() {
    let root = Root::new();
    let mut options = root.options();
    for sharing in ["own-user", "tailnet", "tailnet"] {
        options.sharing_scope = sharing.into();
        let report = install(&options).unwrap();
        assert!(!report.dry_run);
        assert_eq!(report.durable, Some(true));
        assert_eq!(
            fs::read_to_string(&report.unit_path).unwrap(),
            report.unit_content
        );
        let metadata = fs::symlink_metadata(&report.unit_path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(metadata.mode() & 0o022, 0);
        no_private_stages(&options);
    }
}

#[test]
fn a_legacy_staging_symlink_cannot_truncate_its_target_or_become_the_unit() {
    let root = Root::new();
    let options = root.options();
    create_unit_directory(&options);
    let unit = resolve_unit_path(&options);
    let legacy = unit.with_extension("tmp");
    let victim = root.0.join("unrelated-user-file");
    let preserved = b"preserve this existing file byte-for-byte";
    fs::write(&victim, preserved).unwrap();
    symlink(&victim, &legacy).unwrap();

    let report = install(&options).unwrap();
    assert_eq!(fs::read(&victim).unwrap(), preserved);
    assert_eq!(fs::read_link(&legacy).unwrap(), victim);
    assert!(fs::symlink_metadata(&unit).unwrap().is_file());
    assert_eq!(fs::read_to_string(&unit).unwrap(), report.unit_content);
    no_private_stages(&options);
}

#[test]
fn a_legacy_staging_hardlink_remains_untouched() {
    let root = Root::new();
    let options = root.options();
    create_unit_directory(&options);
    let unit = resolve_unit_path(&options);
    let legacy = unit.with_extension("tmp");
    let victim = root.0.join("unrelated-user-file");
    let preserved = b"a hardlink must not become an installer write capability";
    fs::write(&victim, preserved).unwrap();
    fs::hard_link(&victim, &legacy).unwrap();
    let original = fs::metadata(&victim).unwrap();

    let report = install(&options).unwrap();
    assert_eq!(fs::read(&victim).unwrap(), preserved);
    assert_eq!(fs::read(&legacy).unwrap(), preserved);
    assert_eq!(fs::metadata(&legacy).unwrap().ino(), original.ino());
    assert_ne!(fs::metadata(&unit).unwrap().ino(), original.ino());
    assert_eq!(fs::read_to_string(&unit).unwrap(), report.unit_content);
    no_private_stages(&options);
}

#[test]
fn linked_destinations_are_refused_without_replacing_links_or_changing_targets() {
    for hardlink in [false, true] {
        let root = Root::new();
        let options = root.options();
        create_unit_directory(&options);
        let unit = resolve_unit_path(&options);
        let victim = root.0.join("unrelated-user-file");
        fs::write(&victim, b"unchanged").unwrap();
        if hardlink {
            fs::hard_link(&victim, &unit).unwrap();
        } else {
            symlink(&victim, &unit).unwrap();
        }
        let original = fs::symlink_metadata(&unit).unwrap();

        assert_eq!(install(&options), Err(ServiceError::UnsafeUnitPath));
        assert_eq!(uninstall(&options), Err(ServiceError::UnsafeUnitPath));
        let retained = fs::symlink_metadata(&unit).unwrap();
        assert_eq!(retained.ino(), original.ino());
        assert_eq!(retained.file_type(), original.file_type());
        assert_eq!(fs::read(&victim).unwrap(), b"unchanged");
        no_private_stages(&options);
    }
}

#[test]
fn symlink_ancestors_do_not_redirect_directory_creation_or_publication() {
    let root = Root::new();
    let mut options = root.options();
    let outside = root.0.join("outside");
    fs::DirBuilder::new().mode(0o700).create(&outside).unwrap();
    let existing = outside.join("frd.service");
    fs::write(&existing, b"an unrelated service definition").unwrap();
    let linked = root.0.join("linked");
    symlink(&outside, &linked).unwrap();
    options.custom_unit_dir = Some(linked.join("new-units"));

    assert_eq!(install(&options), Err(ServiceError::UnsafeUnitPath));
    assert!(!outside.join("new-units").exists());
    options.custom_unit_dir = Some(linked.clone());
    assert_eq!(uninstall(&options), Err(ServiceError::UnsafeUnitPath));
    assert_eq!(
        fs::read(&existing).unwrap(),
        b"an unrelated service definition"
    );
    assert_eq!(fs::read_link(&linked).unwrap(), outside);
}

#[test]
fn writable_directories_and_nonregular_or_writable_units_refuse_without_damage() {
    for case in ["parent", "ancestor", "directory", "fifo", "writable-unit"] {
        let root = Root::new();
        let mut options = root.options();
        create_unit_directory(&options);
        let unit = resolve_unit_path(&options);
        match case {
            "parent" => {
                fs::set_permissions(unit_directory(&options), fs::Permissions::from_mode(0o777))
                    .unwrap();
            }
            "ancestor" => {
                let selected = unit_directory(&options).join("nested");
                fs::DirBuilder::new().mode(0o700).create(&selected).unwrap();
                fs::set_permissions(unit_directory(&options), fs::Permissions::from_mode(0o777))
                    .unwrap();
                options.custom_unit_dir = Some(selected);
            }
            "directory" => fs::create_dir(&unit).unwrap(),
            "fifo" => rustix::fs::mkfifoat(
                rustix::fs::CWD,
                &unit,
                rustix::fs::Mode::from_raw_mode(0o600),
            )
            .unwrap(),
            "writable-unit" => {
                fs::write(&unit, b"preserved").unwrap();
                fs::set_permissions(&unit, fs::Permissions::from_mode(0o666)).unwrap();
            }
            _ => unreachable!(),
        }

        assert_eq!(
            install(&options),
            Err(ServiceError::UnsafeUnitPath),
            "{case}"
        );
        assert_eq!(
            uninstall(&options),
            Err(ServiceError::UnsafeUnitPath),
            "{case}"
        );
        if case == "writable-unit" {
            assert_eq!(fs::read(&unit).unwrap(), b"preserved");
        }
        no_private_stages(&options);
    }
}

#[test]
fn uninstall_preview_and_missing_paths_never_create_or_remove_anything() {
    let root = Root::new();
    let mut options = root.options();
    for preview in [false, true] {
        options.dry_run = preview;
        let report = uninstall(&options).unwrap();
        assert!(!report.existed);
        assert_eq!(report.durable, None);
        assert!(!unit_directory(&options).exists());
    }

    options.dry_run = false;
    let installed = install(&options).unwrap();
    options.dry_run = true;
    let preview = uninstall(&options).unwrap();
    assert!(preview.existed);
    assert_eq!(preview.durable, None);
    assert_eq!(
        fs::read_to_string(&preview.unit_path).unwrap(),
        installed.unit_content
    );
    no_private_stages(&options);

    options.dry_run = false;
    let removed = uninstall(&options).unwrap();
    assert!(removed.existed);
    assert_eq!(removed.durable, Some(true));
    assert!(!removed.unit_path.exists());
    let again = uninstall(&options).unwrap();
    assert!(!again.existed);
    assert_eq!(again.durable, None);
    no_private_stages(&options);
}

#[test]
fn concurrent_installers_never_share_a_stage_or_publish_partial_contents() {
    const WRITERS: usize = 8;
    let root = Root::new();
    let original = root.options();
    let seeded = install(&original).unwrap();
    let barrier = Arc::new(Barrier::new(WRITERS + 1));
    let done = Arc::new(AtomicBool::new(false));
    // Each preflight checks a distinct port: port ownership is not the subject
    // of this test. Reserve all of them before any concurrent writer starts.
    let ports: Vec<_> = (0..WRITERS)
        .map(|_| TcpListener::bind(("127.0.0.1", 0)).unwrap())
        .collect();
    let options: Vec<_> = ports
        .iter()
        .enumerate()
        .map(|(index, socket)| InstallOptions {
            service_port: socket.local_addr().unwrap().port(),
            socket_path: Some(PathBuf::from(format!("/{}-{index}", "x".repeat(3500)))),
            ..original.clone()
        })
        .collect();
    let expected = Arc::new(
        std::iter::once(seeded.unit_content)
            .chain(options.iter().map(render_systemd_unit))
            .collect::<Vec<_>>(),
    );
    let reader = {
        let barrier = barrier.clone();
        let done = done.clone();
        let expected = expected.clone();
        let unit = seeded.unit_path;
        thread::spawn(move || {
            barrier.wait();
            let mut reads = 0;
            loop {
                let bytes = fs::read_to_string(&unit).unwrap();
                assert!(
                    expected.contains(&bytes),
                    "only complete revisions may be visible"
                );
                reads += 1;
                if done.load(Ordering::Acquire) {
                    return reads;
                }
                thread::yield_now();
            }
        })
    };
    let writers: Vec<_> = options
        .into_iter()
        .zip(ports)
        .map(|(options, socket)| {
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                drop(socket);
                install(&options)
            })
        })
        .collect();
    let outcomes: Vec<_> = writers.into_iter().map(|w| w.join().unwrap()).collect();
    done.store(true, Ordering::Release);
    assert!(reader.join().unwrap() > 0);
    for outcome in outcomes {
        assert_eq!(outcome.unwrap().durable, Some(true));
    }
    let final_unit = fs::read_to_string(resolve_unit_path(&original)).unwrap();
    assert!(expected[1..].contains(&final_unit));
    no_private_stages(&original);
}
