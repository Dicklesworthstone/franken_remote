//! The Windows host-policy store on a real NTFS volume, through frd's public
//! API: icacls, junctions and hard links plant the unsafe cases. Build with
//! `--features windows-host` for x86_64-pc-windows-gnu and run on Windows.
#![cfg(windows)]
use frd::host_policy::{Approval, Change, Error, Sharing, Store, options::RunOptions};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

/// A private folder under the user's temp directory, removed afterwards (a
/// junction inside is removed as itself, not followed).
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("fr-host-policy-win-{}-{id}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn store(&self, relative: &str) -> Store {
        Store::new(&self.0.join(relative)).unwrap()
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Grants Everyone (S-1-1-0) `rights` (icacls notation) on `path`.
fn grant(path: &Path, rights: &str) {
    let output = Command::new("icacls")
        .arg(path)
        .args(["/grant", &format!("*S-1-1-0:{rights}")])
        .output()
        .unwrap();
    assert!(output.status.success(), "icacls {rights}");
}

#[test]
fn saves_load_back_and_unchanged_saves_say_so() {
    let root = Root::new();
    let store = root.store(r"FrankenRemote\host-policy.json");
    assert_eq!(store.load().unwrap().revision, 0);
    assert!(
        !root.0.join("FrankenRemote").exists(),
        "a load creates nothing"
    );
    let first = store.update(Change::Approval(Approval::Local)).unwrap();
    assert!(first.changed && first.durable);
    assert_eq!(first.policy.revision, 1);
    let second = store.update(Change::Sharing(Sharing::Tailnet)).unwrap();
    assert_eq!(second.policy.revision, 2);
    assert_eq!(second.policy.approval_mode, Approval::Local);
    assert_eq!(store.load().unwrap(), second.policy);
    let same = store.update(Change::Sharing(Sharing::Tailnet)).unwrap();
    assert!(!same.changed && same.durable);
    assert_eq!(same.policy, second.policy);
    // The policy and its lock; no staging file is left behind.
    let mut names: Vec<_> = fs::read_dir(root.0.join("FrankenRemote"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, [".frd-policy.lock", "host-policy.json"]);
}

#[test]
fn a_held_lock_refuses_a_second_writer_without_waiting() {
    let root = Root::new();
    let store = root.store("host.json");
    store.update(Change::Approval(Approval::Local)).unwrap();
    let lock = File::options()
        .read(true)
        .write(true)
        .open(root.0.join(".frd-policy.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    assert_eq!(
        store.update(Change::Sharing(Sharing::Tailnet)),
        Err(Error::Busy)
    );
    assert_eq!(store.load().unwrap().approval_mode, Approval::Local);
    drop(lock);
    let saved = store.update(Change::Sharing(Sharing::Tailnet)).unwrap();
    assert_eq!(saved.policy.revision, 2);
}

#[test]
fn a_reader_holding_the_policy_open_does_not_block_a_save() {
    let root = Root::new();
    let store = root.store("host.json");
    store.update(Change::Approval(Approval::Local)).unwrap();
    // std opens files sharing deletion, as a running host's reload does.
    let reader = File::open(root.0.join("host.json")).unwrap();
    let saved = store.update(Change::Sharing(Sharing::Tailnet)).unwrap();
    assert!(saved.changed && saved.durable);
    drop(reader);
    assert_eq!(store.load().unwrap(), saved.policy);
}

#[test]
fn a_policy_others_can_read_is_refused() {
    let root = Root::new();
    let store = root.store("host.json");
    store.update(Change::Approval(Approval::Local)).unwrap();
    assert!(store.load().is_ok());
    grant(&root.0.join("host.json"), "(R)");
    assert_eq!(store.load(), Err(Error::UnsafePath));
}

#[test]
fn a_policy_directory_others_can_add_to_is_refused() {
    let root = Root::new();
    let store = root.store("host.json");
    store.update(Change::Approval(Approval::Local)).unwrap();
    grant(&root.0, "(AD)");
    assert_eq!(store.load(), Err(Error::UnsafePath));
}

#[test]
fn an_ancestor_others_may_add_to_but_not_empty_is_trusted() {
    let root = Root::new();
    let store = root.store(r"inner\host.json");
    store.update(Change::Approval(Approval::Local)).unwrap();
    // Like a sticky directory: others may add entries but not remove ours.
    grant(&root.0, "(AD,WD)");
    assert_eq!(store.load().unwrap().approval_mode, Approval::Local);
    // Deleting entries of it is another matter.
    grant(&root.0, "(DC)");
    assert_eq!(store.load(), Err(Error::UnsafePath));
}

#[test]
fn a_junction_on_the_path_is_refused() {
    let root = Root::new();
    let real = root.store(r"real\host.json");
    real.update(Change::Approval(Approval::Local)).unwrap();
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(root.0.join("linked"))
        .arg(root.0.join("real"))
        .output()
        .unwrap()
        .status;
    assert!(status.success());
    assert_eq!(
        root.store(r"linked\host.json").load(),
        Err(Error::UnsafePath)
    );
    assert_eq!(real.load().unwrap().approval_mode, Approval::Local);
}

#[test]
fn a_hard_linked_policy_is_refused() {
    let root = Root::new();
    let store = root.store("host.json");
    store.update(Change::Approval(Approval::Local)).unwrap();
    fs::hard_link(root.0.join("host.json"), root.0.join("other.json")).unwrap();
    assert_eq!(store.load(), Err(Error::UnsafePath));
}

#[test]
fn an_oversized_policy_is_refused() {
    let root = Root::new();
    let store = root.store("host.json");
    store.update(Change::Approval(Approval::Local)).unwrap();
    fs::write(root.0.join("host.json"), vec![b' '; 4097]).unwrap();
    assert_eq!(store.load(), Err(Error::TooLarge));
}

#[test]
fn only_plain_drive_paths_and_ordinary_names_are_accepted() {
    for refused in [
        r"relative\host.json",
        r"\host.json",
        r"\\server\share\host.json",
        r"\\?\C:\host.json",
        r"C:\dir\..\host.json",
        r"C:\dir\.FRD-POLICY.LOCK",
        r"C:\dir\.frd-policy-pending-1-2",
    ] {
        assert!(Store::new(Path::new(refused)).is_err(), "{refused}");
    }
    assert!(Store::new(Path::new(r"C:\dir\host.json")).is_ok());
}

#[test]
fn the_default_path_is_the_users_local_application_data() {
    let path = frd::host_policy::default_path().unwrap();
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap());
    assert_eq!(path, local.join(r"FrankenRemote\host-policy.json"));
    assert!(Store::new(&path).is_ok());
}

fn run_options(args: &[&str]) -> Result<RunOptions, Error> {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    RunOptions::parse(&args)
}

fn refusal(args: &[&str], approval: Approval) -> Option<&'static str> {
    run_options(args)
        .unwrap()
        .windows_refusal(approval)
        .map(|(code, _)| code)
}

#[test]
fn run_accepts_a_view_only_share_and_refuses_what_windows_lacks() {
    let roots = r"C:\fr\roots.pem";
    let plain = ["--trust-roots", roots];
    assert_eq!(refusal(&plain, Approval::None), None);
    assert_eq!(
        refusal(
            &[
                "--trust-roots",
                roots,
                "--once",
                "--fps",
                "60",
                "--encoder",
                "auto"
            ],
            Approval::None
        ),
        None
    );
    // Without --trust-roots the machine's ROOT certificate store is used.
    assert_eq!(refusal(&[], Approval::None), None);
    // frd injects input itself: --control is the control opt-in, and the
    // controller's clipboard follows it unless --no-clipboard.
    let control = run_options(&["--control"]).unwrap();
    assert!(control.control && control.input_agent.is_none() && control.clipboard);
    assert_eq!(refusal(&["--control"], Approval::None), None);
    assert_eq!(refusal(&["--control", "--clipboard"], Approval::None), None);
    assert!(
        !run_options(&["--control", "--no-clipboard"])
            .unwrap()
            .clipboard
    );
    assert!(
        !run_options(&["--no-clipboard", "--control"])
            .unwrap()
            .clipboard
    );
    // View-only shares never expose the clipboard (frd refuses an explicit
    // --clipboard without --control before any I/O).
    assert!(!run_options(&[]).unwrap().clipboard);
    assert!(run_options(&["--control", "--control"]).is_err());
    // A Windows host prompts on its own desktop for --approval local.
    assert_eq!(refusal(&plain, Approval::Local), None);
    for (extra, code) in [
        (&["--software-explicit"][..], "software_encoder_unavailable"),
        (&["--headless"], "no_display"),
        (&["--display", ":1"], "no_display"),
        (
            &["--input-agent", r"C:\fr\agent.exe"],
            "input_agent_unavailable",
        ),
        (
            &["--input-agent", r"C:\fr\agent.exe", "--microphone"],
            "microphone_endpoint_unavailable",
        ),
        (&["--audio"], "audio_source_unavailable"),
        (
            &["--audio", "--audio-sink", "speakers"],
            "audio_source_unavailable",
        ),
        (&["--logind-session", "1"], "session_monitor_unavailable"),
        (
            &["--logind-session", "1", "--logind-seat", "seat0"],
            "session_monitor_unavailable",
        ),
        (
            &["--observation-indicator", r"C:\fr\indicator.exe"],
            "observation_indicator_unavailable",
        ),
        (
            &["--control", "--microphone"],
            "microphone_endpoint_unavailable",
        ),
    ] {
        let args: Vec<&str> = plain.iter().chain(extra).copied().collect();
        assert_eq!(refusal(&args, Approval::None), Some(code), "{extra:?}");
    }
    // Host file commands are a Linux and macOS protocol; parsing refuses them.
    assert!(
        run_options(&[
            "--trust-roots",
            roots,
            "--once",
            "--input-agent",
            r"C:\fr\agent.exe",
            "--send-control",
            "fr-send",
        ])
        .is_err()
    );
}

#[test]
fn initial_send_reaches_the_pinned_selection_without_an_external_input_agent() {
    use frd::native_files::HostSelection;
    let root = Root::new();
    let source = root.0.join("selected.txt");
    fs::write(&source, b"selected locally").unwrap();
    let options = run_options(&[
        "--encoder",
        "mediafoundation",
        "--once",
        "--control",
        "--approval",
        "local",
        "--send",
        source.to_str().unwrap(),
    ])
    .unwrap();
    assert!(options.once && options.control && options.input_agent.is_none());
    assert_eq!(options.send, [source.clone()]);
    assert_eq!(options.approval, Some(Approval::Local));
    assert!(options.files.is_none() && options.send_control.is_none());
    assert_eq!(options.windows_refusal(Approval::Local), None);
    // This is the public selector called by frd run before desktop/network
    // startup. It opens a native file handle; no transport or control grant is
    // manufactured by accepting the command line or selecting a source.
    let selection = HostSelection::select(&options.send).unwrap();
    assert!(selection.is_available());
    assert!(fs::write(&source, b"replacement").is_err());
    assert!(fs::remove_file(&source).is_err());
    assert!(fs::rename(&source, root.0.join("renamed.txt")).is_err());
    let retained = selection.clone();
    drop(selection);
    assert!(fs::write(&source, b"replacement").is_err());
    assert!(!format!("{retained:?}").contains(source.to_str().unwrap()));
    drop(retained);
    fs::write(&source, b"released after the original owner").unwrap();
}

#[test]
fn initial_send_keeps_control_scope_bounds_and_other_windows_refusals() {
    let source = r"C:\selected\report.pdf";
    for args in [
        vec!["--send", source],
        vec!["--once", "--send", source],
        vec!["--control", "--send", source],
        vec!["--once", "--control", "--send", "relative.txt"],
        vec!["--once", "--control", "--send", r"C:relative.txt"],
        vec!["--once", "--control", "--send", r"\rooted.txt"],
        vec!["--once", "--control", "--send", r"C:\selected\..\file.txt"],
        vec!["--once", "--control", "--send"],
        vec!["--once", "--control", "--send-control", "desk"],
        vec![
            "--once",
            "--control",
            "--send",
            source,
            "--send-control",
            "desk",
        ],
    ] {
        assert!(run_options(&args).is_err(), "{args:?}");
    }
    let mut args = vec!["--once", "--control"];
    for _ in 0..frd::native_files::MAX_SENDS {
        args.extend(["--send", source]);
    }
    assert_eq!(run_options(&args).unwrap().send.len(), 8);
    args.extend(["--send", source]);
    assert!(run_options(&args).is_err());
    assert_eq!(
        refusal(
            &[
                "--once",
                "--control",
                "--send",
                source,
                "--files",
                r"C:\drop"
            ],
            Approval::Local,
        ),
        None,
    );
    assert_eq!(
        refusal(
            &[
                "--once",
                "--input-agent",
                r"C:\fr\agent.exe",
                "--send",
                source,
            ],
            Approval::Local,
        ),
        Some("input_agent_unavailable"),
    );
}

#[test]
fn initial_send_refuses_directories_and_nonordinary_windows_sources() {
    use frd::native_files::{HostSelection, SelectReason};
    let root = Root::new();
    assert_eq!(
        HostSelection::select(std::slice::from_ref(&root.0))
            .unwrap_err()
            .reason,
        SelectReason::Directory,
    );
    for source in [
        r"\\server\share\file.txt",
        r"\\?\C:\selected\file.txt",
        r"\\.\pipe\never-open",
        r"C:\selected\file.txt:stream",
        r"C:\selected\NUL",
    ] {
        let options = run_options(&["--once", "--control", "--send", source]).unwrap();
        let refused = HostSelection::select(&options.send).unwrap_err();
        assert_eq!(refused.index, 0);
        assert_eq!(refused.reason, SelectReason::NotPortable, "{source}");
    }
}

#[test]
fn receive_selects_a_real_private_root_without_starting_transfers_or_granting_control() {
    use frd::native_files::{DirectoryRefusal, Limits};

    let root = Root::new();
    let inbox = root.0.join("inbox");
    fs::create_dir(&inbox).unwrap();
    let options = run_options(&[
        "--control",
        "--files",
        inbox.to_str().unwrap(),
        "--files-conflict",
        "keep-both",
        "--files-max-file-bytes",
        "3",
        "--files-max-session-bytes",
        "6",
    ])
    .unwrap();
    assert!(options.control && options.input_agent.is_none());
    assert_eq!(options.windows_refusal(Approval::Local), None);
    let directory = options.open_files().unwrap().unwrap();
    assert_eq!(
        directory.limits(),
        Limits {
            max_file_bytes: 3,
            max_session_bytes: 6,
        }
    );
    assert!(!format!("{directory:?}").contains(inbox.to_str().unwrap()));
    assert_eq!(fs::read_dir(&inbox).unwrap().count(), 0);
    drop(directory);

    // Native validation checks the actual final directory DACL. The chosen
    // collision policy and positive control option cannot waive that check.
    grant(&inbox, "(AD,WD)");
    assert_eq!(options.open_files().unwrap_err(), DirectoryRefusal::Unsafe);
    assert_eq!(fs::read_dir(&inbox).unwrap().count(), 0);
    assert_eq!(DirectoryRefusal::Unsafe.code(), "files_directory_unsafe");
}

#[test]
fn receive_refuses_reparse_roots_and_nonordinary_windows_names() {
    use frd::native_files::DirectoryRefusal;

    let root = Root::new();
    let real = root.0.join("real");
    fs::create_dir(&real).unwrap();
    let junction = root.0.join("junction");
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&real)
        .output()
        .unwrap()
        .status;
    assert!(status.success());
    let selected = run_options(&["--control", "--files", junction.to_str().unwrap()]).unwrap();
    assert_eq!(selected.open_files().unwrap_err(), DirectoryRefusal::Unsafe);
    assert_eq!(fs::read_dir(&real).unwrap().count(), 0);
    for path in [
        r"\\server\share\inbox",
        r"\\?\C:\inbox",
        r"\\.\pipe\never-open",
        r"C:relative",
        r"\rooted",
        r"C:\selected\..\inbox",
    ] {
        let selected = run_options(&["--control", "--files", path]).unwrap();
        assert_eq!(
            selected.open_files().unwrap_err(),
            DirectoryRefusal::Relative,
            "{path}"
        );
    }
}

#[test]
fn receive_cli_requires_explicit_control_then_uses_the_checked_native_root() {
    let root = Root::new();
    let policy = root.0.join("host.json");
    root.store("host.json")
        .update(Change::Approval(Approval::None))
        .unwrap();
    let missing = root.0.join("receive-destination-must-stay-missing");
    for (control, code) in [
        (false, "files_requires_input_agent"),
        (true, "files_directory_missing"),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_frd"));
        command.args([
            "run",
            "--config",
            policy.to_str().unwrap(),
            "--encoder",
            "mediafoundation",
            "--no-clipboard",
            "--files",
            missing.to_str().unwrap(),
            "--json",
        ]);
        if control {
            command.arg("--control");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{code}");
        let response = String::from_utf8(output.stdout).unwrap();
        assert!(
            response.contains(&format!("\"code\":\"{code}\"")),
            "{response}"
        );
        assert!(!response.contains("receive-destination-must-stay-missing"));
    }
    assert!(!missing.exists());
}
