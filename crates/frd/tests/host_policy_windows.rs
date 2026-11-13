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
    // frd injects input itself: --control is the control opt-in.
    let control = run_options(&["--control"]).unwrap();
    assert!(control.control && control.input_agent.is_none() && !control.clipboard);
    assert_eq!(refusal(&["--control"], Approval::None), None);
    assert!(run_options(&["--control", "--control"]).is_err());
    assert_eq!(
        refusal(&plain, Approval::Local),
        Some("local_approval_unavailable")
    );
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
        (&["--control", "--clipboard"], "clipboard_unavailable"),
        (&["--control", "--files", r"C:\drop"], "files_unavailable"),
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
