use super::*;
use std::{
    fs as disk,
    os::unix::fs::{DirBuilderExt, symlink},
    path::PathBuf,
};

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        let path = std::env::temp_dir().join(format!(
            "frd-unit-stage-{:032x}",
            u128::from_ne_bytes(random)
        ));
        disk::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = disk::remove_dir_all(&self.0);
    }
}

#[test]
fn abandoning_an_owned_stage_removes_it_without_touching_the_installed_unit() {
    let root = Root::new();
    let dir = directory(&root.0, false).unwrap().unwrap();
    let existing = root.0.join("frd.service");
    disk::write(&existing, b"original complete service").unwrap();
    let mut stage = Stage::create(&dir).unwrap();
    let pending = root.0.join(&stage.name);
    stage.file.write_all(b"incomplete replacement").unwrap();
    assert!(pending.is_file());

    drop(stage);
    assert!(!pending.exists());
    assert_eq!(disk::read(&existing).unwrap(), b"original complete service");
}

#[test]
fn refusal_after_staging_preserves_the_new_link_and_retires_only_the_owned_stage() {
    let root = Root::new();
    let dir = directory(&root.0, false).unwrap().unwrap();
    let mut stage = Stage::create(&dir).unwrap();
    let pending = root.0.join(&stage.name);
    stage.file.write_all(b"replacement service").unwrap();
    stage.file.sync_all().unwrap();
    let victim = root.0.join("victim");
    disk::write(&victim, b"preserved").unwrap();
    let unit = root.0.join("frd.service");
    symlink(&victim, &unit).unwrap();

    assert_eq!(
        stage.publish(OsStr::new("frd.service")),
        Err(ServiceError::UnsafeUnitPath)
    );
    assert!(!pending.exists());
    assert_eq!(disk::read_link(&unit).unwrap(), victim);
    assert_eq!(disk::read(&victim).unwrap(), b"preserved");
}

#[test]
fn a_substituted_stage_name_is_neither_published_nor_deleted() {
    let root = Root::new();
    let dir = directory(&root.0, false).unwrap().unwrap();
    let mut stage = Stage::create(&dir).unwrap();
    let pending = root.0.join(&stage.name);
    stage.file.write_all(b"owned stage").unwrap();
    stage.file.sync_all().unwrap();
    let moved = root.0.join("retained-original-stage");
    disk::rename(&pending, &moved).unwrap();
    disk::write(&pending, b"a different file now owns this name").unwrap();

    assert_eq!(
        stage.publish(OsStr::new("frd.service")),
        Err(ServiceError::UnsafeUnitPath)
    );
    assert!(!root.0.join("frd.service").exists());
    assert_eq!(disk::read(&moved).unwrap(), b"owned stage");
    assert_eq!(
        disk::read(&pending).unwrap(),
        b"a different file now owns this name"
    );
}
