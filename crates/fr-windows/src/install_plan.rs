//! The per-user installation layout, computed portably so its paths, the
//! Apps & features registration and the uninstall cleanup are tested on
//! every CI host. The Win32 adapter only executes this plan.
//!
//! Layout (no administrator rights, nothing machine-wide):
//! * `%LOCALAPPDATA%\Programs\FrankenRemote\` holds `FrankenRemote.exe` and,
//!   `fr-opus-worker.exe`, its license and the optional `fr-windows.exe`.
//! * `%APPDATA%\Microsoft\Windows\Start Menu\Programs\FrankenRemote.lnk`.
//! * `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\FrankenRemote`.
//! * `%LOCALAPPDATA%\FrankenRemote\settings.json` (remembered choices).
//!
//! Uninstall removes exactly these names and never a directory's other
//! contents: a directory is removed only once it is empty.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

pub(crate) const PRODUCT: &str = "FrankenRemote";
pub(crate) const APP_EXE: &str = "FrankenRemote.exe";
pub(crate) const CLI_EXE: &str = "fr-windows.exe";
pub(crate) const OPUS_EXE: &str = "fr-opus-worker.exe";
pub(crate) const OPUS_LICENSE: &str = "OPUS-LICENSE.txt";
pub(crate) const SHORTCUT: &str = "FrankenRemote.lnk";
pub(crate) const SETTINGS_FILE: &str = "settings.json";
pub(crate) const UNINSTALL_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\FrankenRemote";
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the program files live, under the user's program-files folder
/// (`FOLDERID_UserProgramFiles`, normally `%LOCALAPPDATA%\Programs`).
pub(crate) fn install_dir(user_programs: &Path) -> PathBuf {
    user_programs.join(PRODUCT)
}

/// The settings directory, under `%LOCALAPPDATA%`.
pub(crate) fn settings_dir(local_app_data: &Path) -> PathBuf {
    local_app_data.join(PRODUCT)
}

/// The Start menu shortcut, under the user's Start menu `Programs` folder.
pub(crate) fn shortcut_path(start_menu_programs: &Path) -> PathBuf {
    start_menu_programs.join(SHORTCUT)
}

/// Windows paths compare case-insensitively and ignore a trailing separator.
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    let normalize = |path: &Path| {
        path.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    normalize(a) == normalize(b)
}

/// One `REG_SZ` or `REG_DWORD` value of the Apps & features registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Text(String),
    Number(u32),
}

/// The values written under `UNINSTALL_KEY`. `size_kib` is the installed
/// files' size, rounded up.
pub(crate) fn registration(dir: &Path, size_bytes: u64) -> Vec<(&'static str, Value)> {
    // Registry values are Windows command lines: join with `\` on every
    // host so the portable tests check the installed form.
    let folder = dir.to_string_lossy();
    let exe = PathBuf::from(format!("{}\\{APP_EXE}", folder.trim_end_matches('\\')));
    let quoted = quote(&exe);
    let size_kib = u32::try_from(size_bytes.div_ceil(1024)).unwrap_or(u32::MAX);
    vec![
        ("DisplayName", Value::Text(PRODUCT.to_owned())),
        ("DisplayVersion", Value::Text(VERSION.to_owned())),
        ("Publisher", Value::Text(PRODUCT.to_owned())),
        ("DisplayIcon", Value::Text(format!("{quoted},0"))),
        (
            "InstallLocation",
            Value::Text(dir.to_string_lossy().into_owned()),
        ),
        (
            "UninstallString",
            Value::Text(format!("{quoted} --uninstall")),
        ),
        (
            "QuietUninstallString",
            Value::Text(format!("{quoted} --uninstall --quiet")),
        ),
        ("NoModify", Value::Number(1)),
        ("NoRepair", Value::Number(1)),
        ("EstimatedSize", Value::Number(size_kib)),
    ]
}

/// A command-line path in double quotes. Windows file names cannot contain
/// `"`, so no escaping is needed (a path that does is refused).
pub(crate) fn quote(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy())
}

/// The deferred cleanup a running uninstaller hands to `cmd.exe`, executed
/// in the install directory itself. Only fixed relative names appear in it
/// (no user path is ever parsed by `cmd`): it waits up to about 30 seconds
/// for the application to exit, deletes the program files, then removes
/// the directory only if it is empty.
pub(crate) fn deferred_cleanup() -> String {
    format!(
        "/d /c \"for /l %i in (1,1,30) do @if exist {APP_EXE} (ping -n 2 127.0.0.1 >nul & del /f /q {APP_EXE} 2>nul) \
         & del /f /q {CLI_EXE} {OPUS_EXE} {OPUS_LICENSE} 2>nul & cd .. & rmdir {PRODUCT} 2>nul\""
    )
}

/// The user `Path` value with `dir` appended, or `None` when an entry
/// already names it (case-insensitive, ignoring a trailing separator and
/// surrounding quotes). Empty entries are dropped from the result.
pub(crate) fn path_with(current: &str, dir: &Path) -> Option<String> {
    if path_entries(current).any(|entry| same_path(Path::new(entry), dir)) {
        return None;
    }
    let mut entries: Vec<&str> = path_entries(current).collect();
    let dir = dir.to_string_lossy();
    entries.push(&dir);
    Some(entries.join(";"))
}

/// The user `Path` value without any entry naming `dir`, or `None` when no
/// entry did (so an unrelated value is never rewritten).
pub(crate) fn path_without(current: &str, dir: &Path) -> Option<String> {
    let kept: Vec<&str> = path_entries(current)
        .filter(|entry| !same_path(Path::new(entry), dir))
        .collect();
    (kept.len() != path_entries(current).count()).then(|| kept.join(";"))
}

fn path_entries(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(';')
        .map(|entry| entry.trim().trim_matches('"'))
        .filter(|entry| !entry.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_per_user_and_fixed() {
        let programs = Path::new(r"C:\Users\me\AppData\Local\Programs");
        let dir = install_dir(programs);
        assert_eq!(dir, programs.join("FrankenRemote"));
        assert_eq!(
            settings_dir(Path::new(r"C:\Users\me\AppData\Local")),
            Path::new(r"C:\Users\me\AppData\Local").join("FrankenRemote")
        );
        assert!(shortcut_path(Path::new("menu")).ends_with("FrankenRemote.lnk"));
        assert!(same_path(
            Path::new(r"C:\Users\Me\AppData\Local\Programs\FrankenRemote\"),
            Path::new("c:/users/me/appdata/local/programs/frankenremote"),
        ));
        assert!(!same_path(
            Path::new(r"C:\a\FrankenRemote"),
            Path::new(r"C:\b\FrankenRemote")
        ));
    }

    #[test]
    fn registration_quotes_paths_and_offers_quiet_uninstall() {
        let dir = Path::new(r"C:\Users\Jane Doe\AppData\Local\Programs\FrankenRemote");
        let values = registration(dir, 10 * 1024 * 1024 + 1);
        let text = |name: &str| match values.iter().find(|(key, _)| *key == name) {
            Some((_, Value::Text(text))) => text.clone(),
            other => panic!("{name}: {other:?}"),
        };
        let exe = format!(r#""{}\FrankenRemote.exe""#, dir.display());
        assert_eq!(text("UninstallString"), format!("{exe} --uninstall"));
        assert_eq!(
            text("QuietUninstallString"),
            format!("{exe} --uninstall --quiet")
        );
        assert_eq!(text("DisplayIcon"), format!("{exe},0"));
        assert_eq!(text("DisplayVersion"), VERSION);
        assert!(values.contains(&("EstimatedSize", Value::Number(10 * 1024 + 1))));
        assert!(values.contains(&("NoModify", Value::Number(1))));
    }

    #[test]
    fn deferred_cleanup_names_only_fixed_relative_files() {
        let script = deferred_cleanup();
        assert!(script.starts_with("/d /c \""));
        assert!(script.ends_with('"'));
        assert!(
            !script.contains(":\\") && !script.contains("/s"),
            "{script}"
        );
        for name in [
            APP_EXE,
            CLI_EXE,
            OPUS_EXE,
            OPUS_LICENSE,
            "rmdir FrankenRemote",
        ] {
            assert!(script.contains(name), "{script}");
        }
        // The only quotes are the outer pair around the /c command.
        assert_eq!(script.matches('"').count(), 2);
    }

    #[test]
    fn the_user_path_gains_and_loses_exactly_one_entry() {
        let dir = Path::new(r"C:\Users\me\AppData\Local\Programs\FrankenRemote");
        assert_eq!(path_with("", dir).as_deref(), Some(dir.to_str().unwrap()));
        let current = r"C:\Tools;;%USERPROFILE%\bin";
        let added = path_with(current, dir).unwrap();
        assert_eq!(
            added,
            format!(r"C:\Tools;%USERPROFILE%\bin;{}", dir.display())
        );
        assert_eq!(path_with(&added, dir), None);
        assert_eq!(
            path_with(
                r#"C:\Tools;"c:\users\ME\appdata\local\programs\frankenremote\""#,
                dir
            ),
            None
        );
        assert_eq!(
            path_without(&added, dir).as_deref(),
            Some(r"C:\Tools;%USERPROFILE%\bin")
        );
        assert_eq!(path_without(current, dir), None);
        assert_eq!(
            path_without(dir.to_str().unwrap(), dir).as_deref(),
            Some("")
        );
    }
}
