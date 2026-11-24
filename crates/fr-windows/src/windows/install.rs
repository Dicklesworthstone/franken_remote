//! Executes the per-user installation plan in [`crate::install_plan`]:
//! program files under the user's program-files folder, a Start menu
//! shortcut, an Apps & features entry, and the folder on the user's `Path`.
//! Nothing here needs administrator rights or writes outside the user's
//! profile and `HKEY_CURRENT_USER`.
use crate::install_plan::{
    self, APP_EXE, CLI_EXE, OPUS_EXE, OPUS_LICENSE, PRODUCT, SETTINGS_FILE, UNINSTALL_KEY, Value,
};
use std::{
    num::NonZeroU16,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
};
use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, LPARAM, WPARAM},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoTaskMemFree, CoUninitialize, IPersistFile,
            },
            Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WRITE, REG_DWORD,
                REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_VALUE_TYPE, RegCloseKey,
                RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
                RegSetValueExW,
            },
            SystemInformation::GetSystemDirectoryW,
        },
        UI::{
            Shell::{
                FOLDERID_LocalAppData, FOLDERID_Programs, FOLDERID_UserProgramFiles, IShellLinkW,
                KF_FLAG_CREATE, SHGetKnownFolderPath, ShellLink,
            },
            WindowsAndMessaging::{
                HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
            },
        },
    },
    core::{GUID, HSTRING, Interface, PCWSTR, w},
};

/// `CREATE_NO_WINDOW`: the deferred cleanup runs without a console.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: the shell allocates the returned string; it is copied and
    // freed here exactly once.
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_CREATE, None).ok()?;
        let owned = path.to_string().ok();
        CoTaskMemFree(Some(path.0.cast_const().cast()));
        owned.filter(|text| !text.is_empty()).map(PathBuf::from)
    }
}

pub(super) fn install_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_UserProgramFiles).map(|programs| install_plan::install_dir(&programs))
}

pub(super) fn settings_path() -> Option<PathBuf> {
    known_folder(&FOLDERID_LocalAppData)
        .map(|local| install_plan::settings_dir(&local).join(SETTINGS_FILE))
}

fn shortcut() -> Option<PathBuf> {
    known_folder(&FOLDERID_Programs).map(|menu| install_plan::shortcut_path(&menu))
}

/// The running executable's folder is the installation folder.
pub(super) fn running_installed() -> bool {
    let current = std::env::current_exe().ok();
    match (current.as_deref().and_then(Path::parent), install_dir()) {
        (Some(running), Some(dir)) => install_plan::same_path(running, &dir),
        _ => false,
    }
}

/// Install or repair this copy for the current user. Returns the installed
/// application's path.
pub(super) fn install() -> Result<PathBuf, String> {
    let current = std::env::current_exe()
        .map_err(|error| format!("this program's location is unknown ({error})"))?;
    let source = current
        .parent()
        .ok_or_else(|| "this program's folder is unknown".to_owned())?;
    let dir = install_dir().ok_or_else(|| "the user's program folder is unavailable".to_owned())?;
    if install_plan::quote(&dir).matches('"').count() != 2 {
        return Err("the program folder's path is not supported".to_owned());
    }
    let decoder = source.join(OPUS_EXE);
    if !decoder.is_file() {
        return Err(format!(
            "the matching {OPUS_EXE} is missing beside this application; install the complete package"
        ));
    }
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("could not create {} ({error})", dir.display()))?;
    if !install_plan::same_path(source, &dir) {
        copy(&decoder, &dir.join(OPUS_EXE))?;
        let license = source.join(OPUS_LICENSE);
        if license.is_file() {
            copy(&license, &dir.join(OPUS_LICENSE))?;
        }
        copy(&current, &dir.join(APP_EXE))?;
        let cli = source.join(CLI_EXE);
        if cli.is_file() {
            copy(&cli, &dir.join(CLI_EXE))?;
        }
    }
    let app = dir.join(APP_EXE);
    let size = [APP_EXE, CLI_EXE, OPUS_EXE, OPUS_LICENSE]
        .iter()
        .filter_map(|name| std::fs::metadata(dir.join(name)).ok())
        .map(|metadata| metadata.len())
        .sum();
    let link = shortcut().ok_or_else(|| "the Start menu folder is unavailable".to_owned())?;
    create_shortcut(&link, &app, &dir)
        .map_err(|error| format!("could not create the Start menu shortcut ({error})"))?;
    register(&install_plan::registration(&dir, size))
        .map_err(|error| format!("could not register with Apps & features ({error})"))?;
    // The command-line viewer on the user's Path (best effort: a refused
    // environment write leaves the application installed and working).
    if dir.join(CLI_EXE).is_file() {
        let _ = edit_user_path(|current| install_plan::path_with(current, &dir));
    }
    Ok(app)
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::copy(from, to).map(|_| ()).map_err(|error| {
        if error.raw_os_error() == Some(32) {
            format!(
                "{} is in use. Close the running FrankenRemote and try again.",
                to.display()
            )
        } else {
            format!("could not copy to {} ({error})", to.display())
        }
    })
}

/// Start the installed copy (after installing from elsewhere).
pub(super) fn launch(app: &Path, port: NonZeroU16) -> bool {
    Command::new(app)
        .args(crate::options::AppCommand::picker_args(port))
        .spawn()
        .is_ok()
}

/// Remove everything this installation created. The running installed copy
/// cannot delete its own program files, so their folder is returned for
/// [`remove_later`] instead.
pub(super) fn uninstall() -> Result<Option<PathBuf>, String> {
    let dir = install_dir().ok_or_else(|| "the user's program folder is unavailable".to_owned())?;
    if let Some(link) = shortcut() {
        remove_file(&link)?;
    }
    let _ = edit_user_path(|current| install_plan::path_without(current, &dir));
    let key = HSTRING::from(UNINSTALL_KEY);
    // SAFETY: deletes only this product's fixed subkey of the user's hive.
    let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &key) };
    if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
        return Err(format!(
            "could not remove the Apps & features entry ({})",
            status.0
        ));
    }
    if let Some(settings) = settings_path() {
        remove_file(&settings)?;
        if let Some(parent) = settings.parent() {
            // Only an empty folder is removed.
            let _ = std::fs::remove_dir(parent);
        }
    }
    if running_installed() {
        return Ok(Some(dir));
    }
    for name in [APP_EXE, CLI_EXE, OPUS_EXE, OPUS_LICENSE] {
        remove_file(&dir.join(name))?;
    }
    let _ = std::fs::remove_dir(&dir);
    Ok(None)
}

/// Hand the program files to a console-less `cmd.exe` that deletes them
/// once this process has exited (see [`install_plan::deferred_cleanup`]).
pub(super) fn remove_later(dir: &Path) -> Result<(), String> {
    let cmd = system_cmd().ok_or_else(|| "cmd.exe was not found".to_owned())?;
    Command::new(cmd)
        .raw_arg(install_plan::deferred_cleanup())
        .current_dir(dir)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(drop)
        .map_err(|error| format!("could not schedule the program files' removal ({error})"))
}

fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not remove {} ({error})", path.display())),
    }
}

fn system_cmd() -> Option<PathBuf> {
    let mut buffer = [0_u16; 260];
    // SAFETY: a bounded write into this stack buffer.
    let length = usize::try_from(unsafe { GetSystemDirectoryW(Some(&mut buffer)) }).ok()?;
    if length == 0 || length >= buffer.len() {
        return None;
    }
    Some(PathBuf::from(String::from_utf16(&buffer[..length]).ok()?).join("cmd.exe"))
}

/// Shell links are created on a short-lived single-threaded apartment so
/// the caller's COM state is never changed.
fn create_shortcut(link: &Path, app: &Path, dir: &Path) -> Result<(), String> {
    let (link, app, dir) = (link.to_owned(), app.to_owned(), dir.to_owned());
    std::thread::spawn(move || {
        // SAFETY: this new thread owns its apartment; the link object is
        // released before CoUninitialize.
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                .ok()
                .map_err(|error| error.to_string())?;
            let result = (|| -> windows::core::Result<()> {
                let shell: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
                shell.SetPath(&HSTRING::from(app.as_os_str()))?;
                shell.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
                shell.SetDescription(w!("Connect to machines on your tailnet"))?;
                shell.SetIconLocation(&HSTRING::from(app.as_os_str()), 0)?;
                shell
                    .cast::<IPersistFile>()?
                    .Save(&HSTRING::from(link.as_os_str()), true)
            })();
            CoUninitialize();
            result.map_err(|error| error.to_string())
        }
    })
    .join()
    .unwrap_or_else(|_| Err("the shortcut worker stopped".to_owned()))
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: closes the key this wrapper opened exactly once.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn register(values: &[(&str, Value)]) -> Result<(), String> {
    let mut key = HKEY::default();
    // SAFETY: creates/opens this product's fixed subkey with a writable out
    // handle; the wrapper closes it.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(UNINSTALL_KEY),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &raw mut key,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!("error {}", status.0));
    }
    let key = Key(key);
    for (name, value) in values {
        let (kind, bytes) = match value {
            Value::Text(text) => (windows::Win32::System::Registry::REG_SZ, wide_bytes(text)),
            Value::Number(number) => (REG_DWORD, number.to_le_bytes().to_vec()),
        };
        // SAFETY: the value bytes outlive the synchronous call.
        let status =
            unsafe { RegSetValueExW(key.0, &HSTRING::from(*name), None, kind, Some(&bytes)) };
        if status != ERROR_SUCCESS {
            return Err(format!("{name}: error {}", status.0));
        }
    }
    Ok(())
}

fn wide_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// Rewrite `HKCU\Environment\Path` when `edit` returns a new value, keeping
/// its registry type, then tell running programs that the environment
/// changed. An absent value is edited as empty.
fn edit_user_path(edit: impl FnOnce(&str) -> Option<String>) -> Result<(), String> {
    let mut key = HKEY::default();
    // SAFETY: opens the user's own environment key with a writable out handle.
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Environment"),
            None,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            &raw mut key,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!("error {}", status.0));
    }
    let key = Key(key);
    let (current, kind) = read_text(&key, w!("Path"))?;
    let Some(updated) = edit(&current) else {
        return Ok(());
    };
    // SAFETY: the bytes outlive the synchronous calls on this open key.
    unsafe {
        let status = if updated.is_empty() {
            RegDeleteValueW(key.0, w!("Path"))
        } else {
            RegSetValueExW(key.0, w!("Path"), None, kind, Some(&wide_bytes(&updated)))
        };
        if status != ERROR_SUCCESS {
            return Err(format!("error {}", status.0));
        }
        let mut result = 0_usize;
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("Environment").as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            2_000,
            Some(&raw mut result),
        );
    }
    Ok(())
}

/// A string value and its type (`REG_EXPAND_SZ` when absent, which is what
/// Windows itself uses for the user `Path`).
fn read_text(key: &Key, name: PCWSTR) -> Result<(String, REG_VALUE_TYPE), String> {
    let mut kind = REG_VALUE_TYPE::default();
    let mut size = 0_u32;
    // SAFETY: a size query with writable out parameters.
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name,
            None,
            Some(&raw mut kind),
            None,
            Some(&raw mut size),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok((String::new(), REG_EXPAND_SZ));
    }
    if status != ERROR_SUCCESS || size > 64 * 1024 {
        return Err(format!("error {}", status.0));
    }
    let mut buffer = vec![0_u16; usize::try_from(size).unwrap_or(0).div_ceil(2) + 1];
    let mut bytes = u32::try_from(buffer.len() * 2).unwrap_or(0);
    // SAFETY: the buffer holds `bytes` writable bytes for this call.
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name,
            None,
            Some(&raw mut kind),
            Some(buffer.as_mut_ptr().cast()),
            Some(&raw mut bytes),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!("error {}", status.0));
    }
    if kind != windows::Win32::System::Registry::REG_SZ && kind != REG_EXPAND_SZ {
        return Err("Path is not a string value".to_owned());
    }
    let units = usize::try_from(bytes).unwrap_or(0) / 2;
    let text = String::from_utf16_lossy(&buffer[..units.min(buffer.len())]);
    Ok((text.trim_end_matches('\0').to_owned(), kind))
}

/// The product name for message boxes.
pub(super) const TITLE: &str = PRODUCT;
