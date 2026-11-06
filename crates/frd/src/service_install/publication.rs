//! Linux service publication through the original directory descriptor. No
//! existing file is opened for writing: each invocation owns one exclusive
//! random stage, and readers see one whole service definition after rename.
use super::ServiceError;
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, FileType, Mode, OFlags},
    process::geteuid,
};
use std::{
    ffi::OsStr,
    fs::File,
    io::Write,
    path::{Component, Path},
};

const TEMP_PREFIX: &str = ".frd-unit-pending-";
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub(super) fn write(path: &Path, bytes: &[u8]) -> Result<bool, ServiceError> {
    let (parent, name) = split_path(path)?;
    let dir = directory(parent, true)?.ok_or(ServiceError::UnsafeUnitPath)?;
    destination(&dir, name)?;
    let mut stage = Stage::create(&dir)?;
    stage
        .file
        .write_all(bytes)
        .map_err(|e| io_error("write the private service stage", e))?;
    stage
        .file
        .sync_all()
        .map_err(|e| io_error("synchronize the private service stage", e))?;
    stage.publish(name)
}

/// No directory is created during removal, including a preview or an already
/// absent installation. An unsafe path never becomes an idempotent success.
pub(super) fn remove(path: &Path, preview: bool) -> Result<(bool, Option<bool>), ServiceError> {
    let (parent, name) = split_path(path)?;
    let Some(dir) = directory(parent, false)? else {
        return Ok((false, None));
    };
    let existed = destination(&dir, name)?;
    if preview || !existed {
        return Ok((existed, None));
    }
    match fs::unlinkat(&dir, name, AtFlags::empty()) {
        Ok(()) => Ok((true, Some(fs::fsync(&dir).is_ok()))),
        // Another ordinary uninstall may have completed first.
        Err(rustix::io::Errno::NOENT) => Ok((false, None)),
        Err(e) => Err(io_error("remove the service definition", e)),
    }
}

fn split_path(path: &Path) -> Result<(&Path, &OsStr), ServiceError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return Err(ServiceError::UnsafeUnitPath);
    }
    let parent = path.parent().ok_or(ServiceError::UnsafeUnitPath)?;
    let name = path.file_name().ok_or(ServiceError::UnsafeUnitPath)?;
    Ok((parent, name))
}

fn io_error(operation: &str, error: impl std::fmt::Display) -> ServiceError {
    ServiceError::IoError {
        detail: format!("Failed to {operation}: {error}"),
    }
}

/// Open every component without following links. Descriptor-relative creation
/// and publication cannot be redirected by replacing an ancestor pathname.
fn directory(path: &Path, create: bool) -> Result<Option<OwnedFd>, ServiceError> {
    let mut dir = fs::open("/", DIRECTORY_FLAGS, Mode::empty())
        .map_err(|e| io_error("open the service directory root", e))?;
    for component in path.components() {
        if let Component::Normal(name) = component {
            trusted_directory(&dir, false)?;
            dir = match fs::openat(&dir, name, DIRECTORY_FLAGS, Mode::empty()) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::NOENT) if create => {
                    match fs::mkdirat(&dir, name, Mode::from_raw_mode(0o700)) {
                        Ok(()) => fs::fsync(&dir)
                            .map_err(|e| io_error("synchronize a new service directory", e))?,
                        Err(rustix::io::Errno::EXIST) => {}
                        Err(e) => return Err(io_error("create the service directory", e)),
                    }
                    open_directory(&dir, name)?
                }
                Err(rustix::io::Errno::NOENT) => return Ok(None),
                Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => {
                    return Err(ServiceError::UnsafeUnitPath);
                }
                Err(e) => return Err(io_error("open the service directory", e)),
            };
        }
    }
    trusted_directory(&dir, true)?;
    Ok(Some(dir))
}

fn open_directory(dir: &OwnedFd, name: &OsStr) -> Result<OwnedFd, ServiceError> {
    fs::openat(dir, name, DIRECTORY_FLAGS, Mode::empty()).map_err(|e| match e {
        rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => ServiceError::UnsafeUnitPath,
        e => io_error("open the service directory", e),
    })
}

fn trusted_directory(dir: &OwnedFd, final_directory: bool) -> Result<(), ServiceError> {
    let stat = fs::fstat(dir).map_err(|e| io_error("inspect the service directory", e))?;
    let own = stat.st_uid == geteuid().as_raw();
    // A root-owned sticky temporary root is traversable, but never a writable
    // destination itself. The selected user/root remains an explicit trust limit.
    let sticky_root = stat.st_uid == 0 && stat.st_mode & 0o1000 != 0;
    let group_or_other_writable = stat.st_mode & 0o022 != 0;
    let trusted = if final_directory {
        own && !group_or_other_writable
    } else {
        (own || stat.st_uid == 0) && (!group_or_other_writable || sticky_root)
    };
    if !trusted {
        return Err(ServiceError::UnsafeUnitPath);
    }
    Ok(())
}

fn destination(dir: &OwnedFd, name: &OsStr) -> Result<bool, ServiceError> {
    match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat)
            if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                && stat.st_uid == geteuid().as_raw()
                && stat.st_mode & 0o022 == 0
                && stat.st_nlink == 1 =>
        {
            Ok(true)
        }
        Ok(_) => Err(ServiceError::UnsafeUnitPath),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(e) => Err(io_error("inspect the existing service definition", e)),
    }
}

struct Stage<'a> {
    dir: &'a OwnedFd,
    name: String,
    file: File,
    published: bool,
}
impl<'a> Stage<'a> {
    fn create(dir: &'a OwnedFd) -> Result<Self, ServiceError> {
        for _ in 0..16 {
            let mut random = [0; 16];
            getrandom::fill(&mut random)
                .map_err(|_| io_error("allocate a service stage", "OS randomness unavailable"))?;
            let name = format!("{TEMP_PREFIX}{:032x}", u128::from_ne_bytes(random));
            match fs::openat(
                dir,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            ) {
                Ok(fd) => {
                    return Ok(Self {
                        dir,
                        name,
                        file: File::from(fd),
                        published: false,
                    });
                }
                Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(io_error("create the private service stage", e)),
            }
        }
        Err(io_error(
            "allocate a service stage",
            "collision budget exhausted",
        ))
    }

    fn publish(mut self, name: &OsStr) -> Result<bool, ServiceError> {
        // Revalidate after writing, before the sole publication side effect.
        // Concurrent installers replace whole regular units, never share stages.
        trusted_directory(self.dir, true)?;
        destination(self.dir, name)?;
        if !self.owns_name() {
            return Err(ServiceError::UnsafeUnitPath);
        }
        fs::renameat(self.dir, self.name.as_str(), self.dir, name)
            .map_err(|e| io_error("publish the service definition", e))?;
        self.published = true;
        // Rename already succeeded: preserve that result if crash durability
        // cannot be confirmed, just as the existing host-policy store does.
        Ok(fs::fsync(self.dir).is_ok())
    }

    fn owns_name(&self) -> bool {
        let Ok(opened) = fs::fstat(&self.file) else {
            return false;
        };
        fs::statat(self.dir, self.name.as_str(), AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|named| {
            named.st_dev == opened.st_dev
                && named.st_ino == opened.st_ino
                && named.st_nlink == 1
                && named.st_uid == geteuid().as_raw()
                && !Mode::from_raw_mode(named.st_mode).intersects(Mode::RWXG | Mode::RWXO)
                && FileType::from_raw_mode(named.st_mode) == FileType::RegularFile
        })
    }
}
impl Drop for Stage<'_> {
    fn drop(&mut self) {
        if !self.published && self.owns_name() {
            let _ = fs::unlinkat(self.dir, self.name.as_str(), AtFlags::empty());
        }
    }
}

#[cfg(test)]
mod tests;
