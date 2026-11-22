//! Private scratch directories that a crash can leave behind.
//!
//! A boot operation builds a capture, a UKI, or a kernel-install tree in a
//! scratch directory named with a fixed prefix. The process that owns the
//! directory holds an exclusive `flock` on it for as long as it works there.
//! An interrupted process loses the lock when it exits, so the next
//! operation that creates scratch in the same parent removes every entry
//! with that prefix whose lock it can take, and leaves the ones that a
//! running process still holds.

use std::{
    fs::{self, File},
    path::Path,
};

use anyhow::{Context, Result};
use rustix::{
    fs::{FlockOperation, flock},
    io::Errno,
};
use tempfile::TempDir;

/// A scratch directory leased to this process.
pub(super) struct Scratch {
    directory: TempDir,
    _lease: File,
}

impl Scratch {
    /// Create a scratch directory in `parent`, first removing the stale ones
    /// that interrupted runs left there. `remove` deletes one stale
    /// directory's contents that `remove_dir_all` cannot, such as a Btrfs
    /// subvolume, before the directory itself is removed.
    pub(super) fn create(
        parent: &Path,
        prefix: &str,
        remove: impl Fn(&Path) -> Result<()>,
    ) -> Result<Self> {
        sweep(parent, prefix, remove)?;
        let directory = tempfile::Builder::new().prefix(prefix).tempdir_in(parent)?;
        let lease = File::open(directory.path())?;
        flock(&lease, FlockOperation::NonBlockingLockExclusive)?;
        Ok(Self {
            directory,
            _lease: lease,
        })
    }

    pub(super) fn path(&self) -> &Path {
        self.directory.path()
    }
}

/// Remove the scratch directories with `prefix` in `parent` that no running
/// process holds, and return how many were removed.
pub(super) fn sweep(
    parent: &Path,
    prefix: &str,
    remove: impl Fn(&Path) -> Result<()>,
) -> Result<usize> {
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("listing {}", parent.display()));
        }
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_str().is_some_and(|name| name.starts_with(prefix))
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let path = entry.path();
        let lease = File::open(&path)?;
        match flock(&lease, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(Errno::WOULDBLOCK) => continue,
            Err(error) => return Err(error.into()),
        }
        remove(&path).with_context(|| format!("removing stale scratch {}", path.display()))?;
        fs::remove_dir_all(&path)
            .with_context(|| format!("removing stale scratch {}", path.display()))?;
        removed += 1;
    }
    Ok(removed)
}

/// Nothing to remove before `remove_dir_all`.
pub(super) fn plain(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweeps_only_scratch_that_no_process_holds() -> Result<()> {
        let parent = tempfile::tempdir()?;
        // An interrupted run left one scratch directory behind.
        let stale = parent.path().join(".cps-test-stale");
        fs::create_dir(&stale)?;
        fs::write(stale.join("partial"), "data")?;
        let unrelated = parent.path().join("keep");
        fs::create_dir(&unrelated)?;
        let held = Scratch::create(parent.path(), ".cps-test-", plain)?;
        assert!(!stale.exists());
        assert!(unrelated.exists());
        // A second run leaves the first run's scratch while it is held.
        let second = Scratch::create(parent.path(), ".cps-test-", plain)?;
        assert!(held.path().exists());
        assert_eq!(sweep(parent.path(), ".cps-test-", plain)?, 0);
        assert!(second.path().exists());
        Ok(())
    }
}
