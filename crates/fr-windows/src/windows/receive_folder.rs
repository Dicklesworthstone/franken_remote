//! Native local-folder selection before any remote connection exists.
//! The common dialog runs on the picker UI thread, owns no session or lease,
//! and returns only an exact bounded pathname. The existing receive owner
//! still validates and pins the directory before starting the connection.
use crate::file_choice::{Error, MAX_PATH_UNITS};
use windows::{
    Win32::{
        Foundation::{ERROR_CANCELLED, HWND},
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
            CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
        },
        UI::Shell::{
            FOS_DONTADDTORECENT, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_NODEREFERENCELINKS,
            FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
        },
    },
    core::{HRESULT, PWSTR, w},
};

struct Apartment;

impl Apartment {
    fn enter() -> Result<Self, Error> {
        // SAFETY: this function and every dialog call run on the same UI
        // thread. Both S_OK and S_FALSE own one matching CoUninitialize.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }
            .ok()
            .map_err(|_| Error::Native)?;
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful initialization on this same thread;
        // all dialog and shell-item interfaces are dropped before Apartment.
        unsafe { CoUninitialize() };
    }
}

struct ReturnedPath(PWSTR);

impl Drop for ReturnedPath {
    fn drop(&mut self) {
        // SAFETY: IShellItem::GetDisplayName allocated this one pointer with
        // the COM allocator; it has one owner and is no longer read after Drop.
        unsafe { CoTaskMemFree(Some(self.0.0.cast_const().cast())) };
    }
}

pub(super) fn choose(owner: HWND) -> Result<Option<Vec<u16>>, Error> {
    let _apartment = Apartment::enter()?;
    // SAFETY: the system common dialog's interfaces are created, invoked and
    // released on this initialized apartment. Win32 owns its child windows;
    // Show returns only after its modal UI is closed. The parent is this
    // thread's live machine picker and no application borrow spans Show.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|_| Error::Native)?;
        let options = dialog.GetOptions().map_err(|_| Error::Native)?;
        dialog
            .SetOptions(
                options
                    | FOS_PICKFOLDERS
                    | FOS_FORCEFILESYSTEM
                    | FOS_PATHMUSTEXIST
                    | FOS_NOCHANGEDIR
                    | FOS_NODEREFERENCELINKS
                    | FOS_DONTADDTORECENT,
            )
            .map_err(|_| Error::Native)?;
        dialog
            .SetTitle(w!("Receive host files for this connection — FrankenRemote"))
            .map_err(|_| Error::Native)?;
        dialog
            .SetOkButtonLabel(w!("Use this folder"))
            .map_err(|_| Error::Native)?;
        if let Err(error) = dialog.Show(Some(owner)) {
            return if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
                Ok(None)
            } else {
                Err(Error::Native)
            };
        }
        let item = dialog.GetResult().map_err(|_| Error::Native)?;
        let path = ReturnedPath(
            item.GetDisplayName(SIGDN_FILESYSPATH)
                .map_err(|_| Error::InvalidPath)?,
        );
        if path.0.is_null() {
            return Err(Error::InvalidPath);
        }
        let mut units = Vec::new();
        // GetDisplayName guarantees one NUL-terminated UTF-16 allocation.
        // Read only its prefix through that terminator, with a fixed upper
        // limit before every push; do not use a lossy/unbounded conversion.
        for index in 0..MAX_PATH_UNITS {
            let unit = path.0.0.add(index).read();
            units.push(unit);
            if unit == 0 {
                return Ok(Some(units));
            }
        }
        Err(Error::InvalidPath)
    }
}
