//! Bounded selection of a locally supplied directory descriptor. Never reopen a
//! child by an absolute/cwd path, and never follow links or open special devices.
use super::{Error, File, Gate, Identity, identity, unchanged};
use crate::receive::directory::{
    MAX_TREE_ENTRIES, MAX_TREE_NAMES_BYTES, MAX_TREE_NODES, path_components,
};
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};

pub(super) struct Selected {
    pub file: File,
    pub path: String,
    pub original: Identity,
}
pub(super) struct Selection {
    pub files: Vec<Selected>,
    // Retain and recheck directory identity/mtime/ctime as well as every file.
    // A changing tree is refused, not silently described as a stable snapshot.
    directories: Vec<(File, Identity)>,
    bytes: u64,
    names: usize,
}
impl Selection {
    pub(super) fn new(
        file: File,
        name: &str,
        directory: bool,
        maximum: u64,
        gate: &Gate,
    ) -> Result<Self, Error> {
        gate.check()?;
        let m = file.metadata()?;
        let original = identity(&m);
        let mut selection = Self {
            files: Vec::new(),
            directories: Vec::new(),
            bytes: 0,
            names: name.len(),
        };
        if directory {
            if !m.is_dir() {
                return Err(Error::Source);
            }
            selection.directories.push((file, original));
            selection.visit(0, "", maximum, gate)?;
            selection.files.sort_by(|a, b| a.path.cmp(&b.path));
        } else {
            if !m.is_file() || m.len() > maximum {
                return Err(Error::Source);
            }
            selection.bytes = m.len();
            selection.files.push(Selected {
                file,
                path: name.to_owned(),
                original,
            });
        }
        selection.check(gate)?;
        Ok(selection)
    }
    fn visit(
        &mut self,
        parent: usize,
        prefix: &str,
        maximum: u64,
        gate: &Gate,
    ) -> Result<(), Error> {
        let before = self.files.len();
        let mut iterator =
            fs::Dir::read_from(&self.directories[parent].0).map_err(std::io::Error::from)?;
        while let Some(entry) = iterator.read() {
            gate.check()?;
            let entry = entry.map_err(std::io::Error::from)?;
            let name = entry.file_name().to_str().map_err(|_| Error::Name)?;
            if matches!(name, "." | "..") {
                continue;
            }
            crate::receive::validate_name(name).map_err(|_| Error::Name)?;
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            path_components(&path).map_err(|_| Error::Limits)?;
            if self.files.len() + self.directories.len() > MAX_TREE_NODES {
                return Err(Error::Limits);
            }
            self.names = self.names.checked_add(path.len()).ok_or(Error::Limits)?;
            if self.names > MAX_TREE_NAMES_BYTES {
                return Err(Error::Limits);
            }
            let parent_fd = &self.directories[parent].0;
            let stat = fs::statat(parent_fd, name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(std::io::Error::from)?;
            let kind = FileType::from_raw_mode(stat.st_mode);
            if !matches!(kind, FileType::RegularFile | FileType::Directory) {
                return Err(Error::Source);
            }
            let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
            if kind == FileType::Directory {
                flags |= OFlags::DIRECTORY;
            }
            let file: File = fs::openat(parent_fd, name, flags, Mode::empty())
                .map_err(std::io::Error::from)?
                .into();
            let m = file.metadata()?;
            // Detect replacement between no-follow classification and opening.
            // NONBLOCK also prevents a racing FIFO from hanging this disk owner.
            if m.dev() != stat.st_dev || m.ino() != stat.st_ino {
                return Err(Error::SourceChanged);
            }
            let original = identity(&m);
            if kind == FileType::Directory {
                if !m.is_dir() {
                    return Err(Error::SourceChanged);
                }
                let index = self.directories.len();
                self.directories.push((file, original));
                self.visit(index, &path, maximum, gate)?;
            } else {
                if !m.is_file() {
                    return Err(Error::SourceChanged);
                }
                if self.files.len() >= MAX_TREE_ENTRIES {
                    return Err(Error::Limits);
                }
                self.bytes = self.bytes.checked_add(m.len()).ok_or(Error::Limits)?;
                if self.bytes > maximum {
                    return Err(Error::Limits);
                }
                self.files.push(Selected {
                    file,
                    path,
                    original,
                });
            }
        }
        unchanged(&self.directories[parent].0, &self.directories[parent].1)?;
        // The portable ATP content graph cannot represent empty non-root dirs.
        // Refuse rather than silently deleting that part of a user's selection.
        if parent != 0 && self.files.len() == before {
            return Err(Error::Source);
        }
        Ok(())
    }
    pub(super) fn check(&self, gate: &Gate) -> Result<(), Error> {
        gate.check()?;
        for (fd, original) in &self.directories {
            unchanged(fd, original)?;
        }
        for entry in &self.files {
            unchanged(&entry.file, &entry.original)?;
        }
        gate.check()
    }
    pub(super) fn total_bytes(&self) -> u64 {
        self.bytes
    }
}
use std::os::unix::fs::MetadataExt;
