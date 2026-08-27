//! The resource on disk.
//!
//! Never written in place. Content is built in a temporary file and then `rename`d over the target
//! (handover §7.4c). Two distinct benefits, worth keeping separate because they are easy to
//! conflate:
//!
//! 1. **Crash safety — settled.** `rename` is atomic with respect to readers, so a harness that
//!    dies mid-write leaves no partial file: a reader sees the old content or the new, never a
//!    mixture. Caveats: same filesystem only (a cross-filesystem rename is a copy, not atomic),
//!    and atomicity is not durability — that additionally needs `fsync` of the temp file and then
//!    of the directory.
//! 2. **Shrinking the check-to-commit gap — partial.** Revocation does not *interrupt* a write;
//!    the harness keeps running. The real hazard is a write that *succeeds* even though the lock
//!    was revoked partway through. Splitting into prepare (no effect on the resource) and commit
//!    (one instantaneous rename) shrinks that exposure from "however long writing the file takes"
//!    to one syscall. **Shrinking is not closing** — see `doc/agent-model.md` §5.4b, where it is
//!    recorded as open, and why putting the version in the lock service is what actually closes it.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};

pub struct FileStore {
    path: PathBuf,
    /// Injected failure point, for R13. `None` in normal operation.
    fail_at: Option<CrashPoint>,
}

/// Where to abort, for R13. Deterministic — we choose the point, so this enumerates injection
/// sites rather than racing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// After the temp file exists, before the rename.
    AfterTempWritten,
    /// Standing in for a rename that does not complete. On a real filesystem this cannot leave a
    /// partial file; modelled here so the property is asserted rather than assumed.
    DuringRename,
}

/// Content hash stored as the version key's value in the lock service, so a file on disk that
/// disagrees with what the service believes is *detectable* rather than silent
/// (`doc/lock-interface.md` §7 D5).
///
/// NOTE: `DefaultHasher` is not cryptographic. It only has to detect change, not resist forgery —
/// agents never supply hashes, the harness computes them.
pub fn content_hash(content: &str) -> u64 {
    let mut h = DefaultHasher::new();
    content.hash(&mut h);
    h.finish()
}

impl FileStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self { path: path.as_ref().to_path_buf(), fail_at: None }
    }

    pub fn with_crash_at(mut self, p: CrashPoint) -> Self {
        self.fail_at = Some(p);
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> io::Result<String> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e),
        }
    }

    /// Stage content into a temp file beside the target. No effect on the resource yet.
    pub fn prepare(&self, content: &str) -> io::Result<PathBuf> {
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, content)?;
        if self.fail_at == Some(CrashPoint::AfterTempWritten) {
            return Err(io::Error::other("injected crash: after temp written"));
        }
        Ok(tmp)
    }

    /// Publish staged content. One syscall — this is the commit, and the whole reason for the split.
    pub fn commit(&self, tmp: &Path) -> io::Result<()> {
        if self.fail_at == Some(CrashPoint::DuringRename) {
            return Err(io::Error::other("injected crash: during rename"));
        }
        std::fs::rename(tmp, &self.path)
    }
}
