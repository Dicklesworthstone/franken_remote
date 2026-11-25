//! One bounded source scan, kept outside the session/authority thread.
use super::{HostConfig, LiveError, paging};
use crate::{
    sender::batch::Selection,
    session::{Authority, Permission},
    sync::SyncError,
};
use asupersync::cx::Cx;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

#[cfg(unix)]
pub(super) use super::super::cache::ScanCache;
#[cfg(not(unix))]
#[derive(Default)]
pub(super) struct ScanCache {}

#[derive(Clone)]
pub(super) struct Version {
    pub(super) name: String,
    pub(super) hash: String,
    pub(super) bytes: u64,
}
pub(super) struct PreparedRound {
    pub(super) selection: Selection,
    pub(super) versions: Vec<Version>,
    pub(super) bytes: u64,
    pub(super) cache: ScanCache,
    pub(super) continuation: paging::Continuation,
}
pub(super) struct Scan {
    thread: Option<JoinHandle<Result<PreparedRound, LiveError>>>,
}
/// The original owner and fixed deadline move together into the disk worker.
pub(super) struct ScanScope {
    pub(super) cx: Cx,
    pub(super) authority: Authority,
    pub(super) permission: Permission,
    pub(super) stopped: Arc<AtomicBool>,
    pub(super) until: u64,
}
impl Scan {
    pub(super) fn start(
        config: &HostConfig,
        revision: u64,
        ledger: BTreeMap<String, String>,
        after: Option<String>,
        cache: ScanCache,
        scope: ScanScope,
    ) -> Result<Self, LiveError> {
        let root = config.root.clone();
        let limits = config.limits;
        let clock = scope.cx.timer_driver().ok_or(LiveError::Clock)?;
        let thread = thread::Builder::new()
            .name("fr-folder-scan".into())
            .spawn(move || {
                let mut check = || {
                    if scope.stopped.load(Ordering::Acquire)
                        || !scope.permission.is_approved()
                        || scope.cx.checkpoint().is_err()
                    {
                        return Err(SyncError::AuthorityRevoked);
                    }
                    let now = clock.now().as_nanos() / 1000;
                    if now >= scope.until
                        || scope
                            .authority
                            .sample(now)
                            .and_then(|at| scope.authority.deadline(at))
                            .is_err()
                    {
                        return Err(SyncError::AuthorityRevoked);
                    }
                    Ok(())
                };
                #[cfg(unix)]
                {
                    let mut cache = cache;
                    let manifest = cache
                        .scan(root.root_fd(), revision, limits, &mut check)
                        .map_err(LiveError::Source)?;
                    let page = paging::select(
                        manifest
                            .entries
                            .iter()
                            .map(|(name, file)| (name.as_str(), file.sha256_hex.as_str())),
                        &ledger,
                        after.as_deref(),
                    );
                    check().map_err(LiveError::Source)?;
                    let mut selection = Selection::new();
                    let mut versions = Vec::new();
                    let mut bytes = 0u64;
                    for name in page.names {
                        check().map_err(LiveError::Source)?;
                        let file = manifest
                            .entries
                            .get(name)
                            .expect("page refers to the original manifest");
                        let selected = super::super::source::select(root.root_fd(), file)
                            .map_err(LiveError::Source)?;
                        selection
                            .push_expected(selected, &file.name, file.size, &file.sha256_hex)
                            .map_err(LiveError::Send)?;
                        bytes = bytes.checked_add(file.size).ok_or(LiveError::Resource)?;
                        versions.push(Version {
                            name: file.name.clone(),
                            hash: file.sha256_hex.clone(),
                            bytes: file.size,
                        });
                    }
                    check().map_err(LiveError::Source)?;
                    Ok(PreparedRound {
                        selection,
                        versions,
                        bytes,
                        cache,
                        continuation: page.continuation,
                    })
                }
                #[cfg(not(unix))]
                {
                    let _ = (root, revision, ledger, after, limits, cache, &mut check);
                    Err(LiveError::Source(SyncError::UnsupportedPlatform))
                }
            })
            .map_err(|_| LiveError::Worker)?;
        Ok(Self {
            thread: Some(thread),
        })
    }
    pub(super) fn poll(&mut self) -> Result<Option<PreparedRound>, LiveError> {
        let Some(thread) = &self.thread else {
            return Ok(None);
        };
        if !thread.is_finished() {
            return Ok(None);
        }
        self.thread
            .take()
            .expect("finished scan retained")
            .join()
            .map_err(|_| LiveError::Panicked)?
            .map(Some)
    }
}
