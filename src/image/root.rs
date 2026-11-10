use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{Read, Seek, Write},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{
        AtFlags, CWD, FileType, Mode, OFlags, RenameFlags, ResolveFlags, Stat, XattrFlags, chownat,
        fchmod, fchown, fgetxattr, flistxattr, fremovexattr, fsetxattr, fstat, ioctl_ficlone,
        linkat, listxattr, mkdirat, open, openat2, readlinkat, renameat, renameat_with, statat,
        symlinkat, unlinkat,
    },
    io::Errno,
    process::{Gid, Uid},
};

use super::{STATE_ANCESTORS, STATE_DIRECTORY};
use crate::{
    file_caps::{ATTRIBUTE, FileCapabilities},
    model::{Node, copy_payload, digest_reader, validate_path},
};

pub struct Root(File);

pub(crate) enum ReplacementSource<'a> {
    Staged(File),
    LocalConfig { path: &'a str, before: &'a Node },
}

/// Stat fields that change whenever a regular file's contents, metadata, or
/// extended attributes change.
///
/// Linux sets the inode change time (ctime) to the current time on every
/// data write, truncation, chmod, chown, link count change, rename, and
/// xattr change, including file capabilities, and userspace cannot set it.
/// A file whose fingerprint still matches one taken while CPS hashed it
/// therefore still has the hashed contents and metadata.
///
/// The device number is deliberately absent. A Btrfs snapshot has a new
/// st_dev but keeps every inode number and ctime, and its contents are the
/// source subvolume's at snapshot time, so fingerprints copied with the
/// image state into a staged snapshot stay valid. Any later change inside
/// the snapshot sets a new ctime there. A file copy, such as an ext4 slot
/// populated from another root, gets new inodes and new ctimes, so its
/// fingerprints never match and CPS hashes those files again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Fingerprint {
    pub(crate) ino: u64,
    pub(crate) size: u64,
    pub(crate) mtime_ns: i64,
    pub(crate) ctime_ns: i64,
    /// The raw st_mode, including the file type.
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

// A fingerprint is only recorded once its ctime is older than this margin
// at hashing time. Filesystems take ctime from a coarse clock, so a change
// in the same clock tick as a recent write could otherwise keep the old
// ctime. With the margin, any later change gets a strictly newer ctime.
// Two seconds also covers filesystems with one-second timestamps.
const SETTLED_NS: i64 = 2_000_000_000;

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_SETTLED_NS: std::cell::Cell<i64> = const { std::cell::Cell::new(SETTLED_NS) };
}

fn settled_ns() -> i64 {
    #[cfg(test)]
    return TEST_SETTLED_NS.with(std::cell::Cell::get);
    #[cfg(not(test))]
    SETTLED_NS
}

fn now_ns() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(now.as_nanos()).unwrap_or(i64::MAX)
}

impl Fingerprint {
    #[allow(clippy::unnecessary_cast)]
    pub(crate) fn of(info: &Stat) -> Self {
        let ns = |seconds: i64, nanoseconds: i64| {
            seconds
                .saturating_mul(1_000_000_000)
                .saturating_add(nanoseconds)
        };
        Self {
            ino: info.st_ino as u64,
            size: info.st_size as u64,
            mtime_ns: ns(info.st_mtime as i64, info.st_mtime_nsec as i64),
            ctime_ns: ns(info.st_ctime as i64, info.st_ctime_nsec as i64),
            mode: info.st_mode as u32,
            uid: info.st_uid,
            gid: info.st_gid,
        }
    }

    fn settled(&self, now: i64) -> bool {
        self.ctime_ns < now.saturating_sub(settled_ns())
    }
}

/// A regular file's hashed node and the fingerprint the file had while CPS
/// hashed it. A matching fingerprint stands in for the hash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Observation {
    pub(crate) fingerprint: Fingerprint,
    pub(crate) sha256: String,
    pub(crate) capabilities: Option<FileCapabilities>,
}

impl Observation {
    pub(crate) fn node(&self) -> Node {
        let fingerprint = &self.fingerprint;
        Node::File {
            sha256: self.sha256.clone(),
            size: fingerprint.size,
            mode: fingerprint.mode & 0o7777,
            uid: fingerprint.uid,
            gid: fingerprint.gid,
            capabilities: self.capabilities,
        }
    }
}

/// The result of inspecting a path with an optional known observation.
pub(crate) struct Inspected {
    pub(crate) node: Option<Node>,
    /// A new settled observation of a regular file that CPS just hashed.
    pub(crate) observed: Option<Observation>,
    /// Whether the known observation's fingerprint matched.
    pub(crate) trusted: bool,
}

/// Directories whose entries changed and still need an fsync.
///
/// Each replacement still syncs its new inode before the rename. The rename
/// itself is atomic, and the pending journal record already exists, so a
/// crash before the directory fsync leaves each path either before or after
/// its write, and resume compares every path against both. Directory syncs
/// can therefore be batched per directory, as long as they complete before
/// the journal records progress past the writes.
#[derive(Default)]
pub(crate) struct DirSyncs {
    directories: BTreeSet<String>,
    staging: bool,
}

impl DirSyncs {
    /// Record that the parent directory of `path` changed.
    pub(crate) fn parent_of(&mut self, path: &str) {
        let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        self.directories.insert(parent.to_owned());
    }

    /// Record that the staging directory changed.
    pub(crate) fn mark_staging(&mut self) {
        self.staging = true;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.directories.is_empty() && !self.staging
    }
}

impl Root {
    pub fn open(path: &Path) -> Result<Self> {
        let fd = open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("opening root {}", path.display()))?;
        Ok(Self(fd.into()))
    }

    pub fn fd_path(file: &File) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
    }

    pub(crate) fn has_xattrs(entry: &File, allow_capability: bool) -> Result<bool> {
        if allow_capability {
            let mut names = [0u8; ATTRIBUTE.len() + 1];
            return match listxattr(Self::fd_path(entry), &mut names[..]) {
                Ok(0) | Err(Errno::NOTSUP) => Ok(false),
                Ok(len) => Ok(&names[..len] != b"security.capability\0"),
                // The attribute name buffer can hold exactly the one
                // declared attribute. A longer list contains other metadata.
                Err(Errno::RANGE) => Ok(true),
                Err(error) => Err(error.into()),
            };
        }
        // An empty buffer queries the size without allocating or reading values.
        let empty: &mut [u8] = &mut [];
        match listxattr(Self::fd_path(entry), empty) {
            Ok(size) => Ok(size != 0),
            Err(Errno::NOTSUP) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn offline_path(&self) -> Result<PathBuf> {
        let image = self.0.metadata()?;
        let host = fs::metadata("/")?;
        ensure!(
            (image.dev(), image.ino()) != (host.dev(), host.ino()),
            "operation requires an inactive image root"
        );
        let path = fs::read_link(Self::fd_path(&self.0))?;
        let current = fs::metadata(&path)?;
        ensure!(
            path.is_absolute() && (current.dev(), current.ino()) == (image.dev(), image.ino()),
            "image root moved or was replaced"
        );
        Ok(path)
    }

    pub fn sync(&self) -> Result<()> {
        Ok(rustix::fs::syncfs(&self.0)?)
    }

    fn open_relative(&self, path: &str, flags: OFlags) -> rustix::io::Result<File> {
        Ok(openat2(
            &self.0,
            path,
            flags | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )?
        .into())
    }

    pub fn dir(&self, path: &str) -> Result<File> {
        validate_path(path)?;
        Ok(self.open_relative(path, OFlags::RDONLY | OFlags::DIRECTORY)?)
    }

    pub(crate) fn subdir(&self, path: &str) -> Result<Self> {
        Ok(Self(self.dir(path)?))
    }

    pub(crate) fn open_image_state(&self) -> Result<Self> {
        let state = self
            .subdir(STATE_DIRECTORY)
            .context("opening image state under /var/lib/cps/image on the image filesystem")?;
        let metadata = state.as_file().metadata()?;
        let device = self.0.metadata()?.dev();
        ensure!(
            metadata.dev() == device && state.dir("stage")?.metadata()?.dev() == device,
            "image state and staging must be on the image's own filesystem and subvolume"
        );
        for &ancestor in STATE_ANCESTORS {
            let parent = self.dir(ancestor)?.metadata()?;
            ensure!(
                parent.uid() == metadata.uid() && parent.mode() & 0o022 == 0,
                "unsafe image state ancestor {ancestor}"
            );
        }
        Ok(state)
    }

    /// Open a Btrfs subvolume beneath a verified top-level volume.
    pub(crate) fn subvolume(&self, path: &str) -> Result<Self> {
        validate_path(path)?;
        Ok(Self(
            openat2(
                &self.0,
                path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
            )?
            .into(),
        ))
    }

    pub(crate) fn ensure_dir(&self, path: &str) -> Result<Self> {
        validate_path(path)?;
        let (parent, name) = self.parent(path)?;
        match mkdirat(&parent, name, Mode::RWXU) {
            Ok(()) => parent.sync_all()?,
            Err(Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
        self.subdir(path)
    }

    pub(crate) fn as_file(&self) -> &File {
        &self.0
    }

    /// Visit entries in sorted depth-first order without following symlinks or crossing a mount.
    /// Returning false from the visitor prunes the entry's descendants.
    pub(crate) fn walk(
        &self,
        skip: Option<&str>,
        mut visit: impl FnMut(&str, &File, &Stat) -> Result<bool>,
    ) -> Result<()> {
        let mut pending = child_paths(&self.0, "")?;
        while let Some(path) = pending.pop() {
            if skip == Some(path.as_str()) {
                continue;
            }
            let entry = self
                .open_relative(&path, OFlags::PATH | OFlags::NOFOLLOW)
                .with_context(|| format!("opening tree entry {path:?}"))?;
            let info = fstat(&entry)?;
            if visit(&path, &entry, &info)?
                && FileType::from_raw_mode(info.st_mode) == FileType::Directory
            {
                pending.extend(child_paths(&entry, &format!("{path}/"))?);
            }
        }
        Ok(())
    }

    pub fn file(&self, path: &str) -> Result<File> {
        validate_path(path)?;
        let file = self.open_relative(path, OFlags::RDONLY | OFlags::NONBLOCK)?;
        ensure!(file.metadata()?.is_file(), "{path} is not a regular file");
        Ok(file)
    }

    pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.file(path)?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn parent<'a>(&self, path: &'a str) -> rustix::io::Result<(File, &'a str)> {
        match path.rsplit_once('/') {
            Some((parent, leaf)) => Ok((
                self.open_relative(parent, OFlags::RDONLY | OFlags::DIRECTORY)?,
                leaf,
            )),
            None => Ok((rustix::io::dup(&self.0)?.into(), path)),
        }
    }

    pub fn inspect(&self, path: &str) -> Result<Option<Node>> {
        validate_path(path)?;
        self.inspect_inner(path)
            .with_context(|| format!("inspecting {path}"))
    }

    pub(crate) fn regular_entry(&self, path: &str) -> Result<Option<File>> {
        validate_path(path)?;
        match self.open_relative(path, OFlags::PATH | OFlags::NOFOLLOW) {
            Ok(file) if file.metadata()?.is_file() => Ok(Some(file)),
            Ok(_) | Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(error).with_context(|| format!("opening hard link path {path}")),
        }
    }

    pub(crate) fn hardlink_matches(&self, path: &str, target: &str) -> Result<bool> {
        let Some(file) = self.regular_entry(target)? else {
            return Ok(false);
        };
        self.shares_inode(path, &file)
    }

    pub(crate) fn resolves_to_file(&self, path: &str, target: &str) -> Result<bool> {
        let file: File = openat2(
            &self.0,
            path,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_XDEV,
        )?
        .into();
        self.shares_inode(target, &file)
    }

    pub(crate) fn shares_inode(&self, path: &str, target: &File) -> Result<bool> {
        let Some(file) = self.regular_entry(path)? else {
            return Ok(false);
        };
        let (observed, expected) = (file.metadata()?, target.metadata()?);
        Ok((observed.dev(), observed.ino()) == (expected.dev(), expected.ino()))
    }

    pub(crate) fn has_other_links(&self, path: &str, aliases: &[&str]) -> Result<bool> {
        let file = self
            .regular_entry(path)?
            .with_context(|| format!("expected a regular file at {path}"))?;
        let links = file.metadata()?.nlink();
        if links == 1 {
            return Ok(false);
        }
        let mut declared = 1;
        for alias in aliases {
            if self.shares_inode(alias, &file)? {
                declared += 1;
            }
        }
        Ok(links != declared)
    }

    pub(crate) fn checked_hardlink_target(&self, target: &str, expected: &Node) -> Result<File> {
        let entry = self
            .regular_entry(target)?
            .with_context(|| format!("hard link target {target} is not a regular file"))?;
        let file = File::open(Self::fd_path(&entry))?;
        ensure!(
            file_node(&file)? == *expected,
            "hard link target {target} differs from its manifest"
        );
        Ok(file)
    }

    fn inspect_inner(&self, path: &str) -> Result<Option<Node>> {
        Ok(self.inspect_known(path, None, false)?.node)
    }

    /// Inspect a path, trusting a known observation whose fingerprint still
    /// matches instead of hashing the file. A hashed regular file returns a
    /// new observation when its fingerprint is settled and did not change
    /// while hashing.
    pub(crate) fn inspect_with(
        &self,
        path: &str,
        known: Option<&Observation>,
    ) -> Result<Inspected> {
        validate_path(path)?;
        self.inspect_known(path, known, true)
            .with_context(|| format!("inspecting {path}"))
    }

    fn inspect_known(
        &self,
        path: &str,
        known: Option<&Observation>,
        observe: bool,
    ) -> Result<Inspected> {
        let absent = Inspected {
            node: None,
            observed: None,
            trusted: false,
        };
        let (parent, leaf) = match self.parent(path) {
            Ok(entry) => entry,
            Err(Errno::NOENT) => return Ok(absent),
            Err(err) => return Err(err.into()),
        };
        let info = match statat(&parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(info) => info,
            Err(Errno::NOENT) => return Ok(absent),
            Err(err) => return Err(err.into()),
        };
        let (uid, gid, mode) = (info.st_uid, info.st_gid, info.st_mode & 0o7777);
        let (mut observed, mut trusted) = (None, false);
        let node = match FileType::from_raw_mode(info.st_mode) {
            FileType::Directory => Node::Dir { uid, gid, mode },
            FileType::Symlink => {
                let target = readlinkat(&parent, leaf, Vec::new())?.into_string()?;
                Node::Symlink { target, uid, gid }
            }
            FileType::RegularFile => match known {
                Some(known) if known.fingerprint == Fingerprint::of(&info) => {
                    trusted = true;
                    known.node()
                }
                _ => {
                    let file = self.file(path)?;
                    let now = now_ns();
                    let before = Fingerprint::of(&fstat(&file)?);
                    let node = file_node(&file)?;
                    if observe
                        && before.settled(now)
                        && before == Fingerprint::of(&fstat(&file)?)
                        && let Node::File {
                            sha256,
                            capabilities,
                            ..
                        } = &node
                    {
                        observed = Some(Observation {
                            fingerprint: before,
                            sha256: sha256.clone(),
                            capabilities: *capabilities,
                        });
                    }
                    node
                }
            },
            _ => Node::Special,
        };
        Ok(Inspected {
            node: Some(node),
            observed,
            trusted,
        })
    }

    /// Open a regular file only if it still has a known fingerprint.
    pub(crate) fn file_matching(&self, path: &str, fingerprint: &Fingerprint) -> Result<File> {
        let file = self.file(path)?;
        ensure!(
            Fingerprint::of(&fstat(&file)?) == *fingerprint,
            "{path} changed after verification"
        );
        Ok(file)
    }

    pub fn replace(
        &self,
        path: &str,
        node: &Node,
        payload: Option<ReplacementSource<'_>>,
        staging: &File,
        require_absent: bool,
        syncs: &mut DirSyncs,
    ) -> Result<()> {
        validate_path(path)?;
        let (parent, leaf) = self.parent(path)?;
        // A crash can leave an incomplete temporary object. The destination
        // is either untouched or has the complete object after rename.
        const TEMP: &str = "write";
        match statat(staging, TEMP, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(info) => unlinkat(
                staging,
                TEMP,
                if FileType::from_raw_mode(info.st_mode) == FileType::Directory {
                    AtFlags::REMOVEDIR
                } else {
                    AtFlags::empty()
                },
            )?,
            Err(Errno::NOENT) => {}
            Err(err) => return Err(err.into()),
        }
        match node {
            Node::File { .. } => {
                let temp = openat2(
                    staging,
                    TEMP,
                    OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                    ResolveFlags::NO_SYMLINKS,
                )?;
                let mut temp = File::from(temp);
                match payload.context("missing file payload")? {
                    ReplacementSource::Staged(mut file) => copy_verified(&mut file, &mut temp)?,
                    ReplacementSource::LocalConfig { path, before } => {
                        self.copy_local_config(path, before, node, &mut temp)?;
                    }
                }
                temp.flush()?;
                Self::set_file_metadata(&temp, node)
                    .with_context(|| format!("preparing file metadata for {path}"))?;
            }
            Node::Dir { .. } => {
                mkdirat(staging, TEMP, Mode::RWXU)?;
                let temp = openat2(
                    staging,
                    TEMP,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                    ResolveFlags::NO_SYMLINKS,
                )?;
                let temp = File::from(temp);
                Self::set_directory_metadata(&temp, node)?;
            }
            Node::Symlink { target, uid, gid } => {
                symlinkat(target.as_str(), staging, TEMP)?;
                let info = statat(staging, TEMP, AtFlags::SYMLINK_NOFOLLOW)?;
                if (info.st_uid, info.st_gid) != (*uid, *gid) {
                    chownat(
                        staging,
                        TEMP,
                        Some(Uid::from_raw(*uid)),
                        Some(Gid::from_raw(*gid)),
                        AtFlags::SYMLINK_NOFOLLOW,
                    )?;
                }
            }
            Node::Hardlink { .. } => {
                let Some(ReplacementSource::Staged(source)) = payload else {
                    anyhow::bail!("missing verified hard link target");
                };
                // Link the checked descriptor, so the target path cannot be
                // substituted between verification and materialization.
                linkat(
                    CWD,
                    Self::fd_path(&source),
                    staging,
                    TEMP,
                    AtFlags::SYMLINK_FOLLOW,
                )?;
            }
            Node::Special => anyhow::bail!("cannot materialize special file {path}"),
        }
        if require_absent {
            renameat_with(staging, TEMP, &parent, leaf, RenameFlags::NOREPLACE)
                .with_context(|| format!("publishing new path {path}"))?;
        } else {
            renameat(staging, TEMP, &parent, leaf)?;
        }
        syncs.parent_of(path);
        syncs.staging = true;
        Ok(())
    }

    fn copy_local_config(
        &self,
        path: &str,
        before: &Node,
        after: &Node,
        temp: &mut File,
    ) -> Result<()> {
        let mut source = self.file(path)?;
        let original = source.metadata()?;
        ensure!(
            before.capabilities().is_none() && after.capabilities().is_none(),
            "unexpected file capabilities on preserved config {path}"
        );
        ensure!(
            file_node(&source)? == *before,
            "local configuration changed at {path}; pending transaction retained"
        );
        source.rewind()?;
        copy_payload(&source, after, temp)
            .with_context(|| format!("copying the local configuration at {path}"))?;
        copy_config_xattrs(&source, temp)
            .with_context(|| format!("preserving extended attributes at {path}"))?;
        let current = source.metadata()?;
        ensure!(
            (original.ctime(), original.ctime_nsec()) == (current.ctime(), current.ctime_nsec())
                && self.shares_inode(path, &source)?,
            "local configuration changed at {path}; pending transaction retained"
        );
        // The caller sets the final mode after copying the ACL, so Linux
        // adjusts the ACL mask to match the package's permissions.
        Ok(())
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        let mut syncs = DirSyncs::default();
        self.remove_file_deferred(path, &mut syncs)?;
        self.sync_dirs(&mut syncs, None)
    }

    /// Remove a file and queue its parent directory for a later fsync.
    pub(crate) fn remove_file_deferred(&self, path: &str, syncs: &mut DirSyncs) -> Result<()> {
        validate_path(path)?;
        let (parent, leaf) = self.parent(path)?;
        unlinkat(&parent, leaf, AtFlags::empty())?;
        syncs.parent_of(path);
        Ok(())
    }

    /// Sync every queued directory once, then the staging directory.
    pub(crate) fn sync_dirs(&self, syncs: &mut DirSyncs, staging: Option<&File>) -> Result<()> {
        // Sync deeper directories first. A queued directory may have been
        // removed later in the same transaction; sync its nearest surviving
        // ancestor, which recorded the removal.
        for directory in std::mem::take(&mut syncs.directories).iter().rev() {
            self.sync_directory(directory)?;
        }
        if std::mem::take(&mut syncs.staging)
            && let Some(staging) = staging
        {
            staging.sync_all()?;
        }
        Ok(())
    }

    fn sync_directory(&self, mut path: &str) -> Result<()> {
        loop {
            if path.is_empty() {
                return Ok(self.0.sync_all()?);
            }
            match self.open_relative(path, OFlags::RDONLY | OFlags::DIRECTORY) {
                Ok(directory) => return Ok(directory.sync_all()?),
                Err(Errno::NOENT) => path = path.rsplit_once('/').map_or("", |(parent, _)| parent),
                Err(error) => {
                    return Err(error).with_context(|| format!("syncing directory {path}"));
                }
            }
        }
    }

    pub(crate) fn set_file_metadata(file: &File, node: &Node) -> Result<()> {
        let Node::File {
            mode,
            uid,
            gid,
            capabilities,
            ..
        } = node
        else {
            anyhow::bail!("expected file metadata");
        };
        set_metadata(file, *mode, *uid, *gid)?;
        // Writes and ownership changes may clear capabilities. Set them only
        // on the finished inode, before syncing and publishing the path.
        if let Some(capabilities) = capabilities {
            capabilities.install(file)?;
        }
        file.sync_all()?;
        Ok(())
    }

    pub fn remove_empty_dir(&self, path: &str, syncs: &mut DirSyncs) -> Result<bool> {
        validate_path(path)?;
        let entry = match self.open_relative(path, OFlags::PATH | OFlags::DIRECTORY) {
            Ok(entry) => entry,
            Err(Errno::XDEV) => return Ok(false),
            Err(error) => return Err(error).with_context(|| format!("opening directory {path}")),
        };
        if Self::has_xattrs(&entry, false)? {
            return Ok(false);
        }
        let (parent, leaf) = self.parent(path)?;
        match unlinkat(&parent, leaf, AtFlags::REMOVEDIR) {
            Ok(()) => {
                syncs.parent_of(path);
                Ok(true)
            }
            Err(Errno::NOTEMPTY | Errno::EXIST | Errno::BUSY) => Ok(false),
            Err(error) => Err(error).with_context(|| format!("pruning directory {path}")),
        }
    }

    pub(crate) fn set_directory_metadata(file: &File, node: &Node) -> Result<()> {
        let Node::Dir { uid, gid, mode } = *node else {
            anyhow::bail!("expected directory metadata");
        };
        set_metadata(file, mode, uid, gid)?;
        file.sync_all()?;
        Ok(())
    }

    pub fn repair_directory(&self, path: &str, before: &Node, after: &Node) -> Result<bool> {
        let Node::Dir { mode: old_mode, .. } = *before else {
            anyhow::bail!("expected directory metadata at {path}");
        };
        let Node::Dir { uid, gid, .. } = *after else {
            anyhow::bail!("expected directory metadata at {path}");
        };
        let file = self
            .dir(path)
            .with_context(|| format!("opening directory {path}"))?;
        let observed = directory_metadata(&file)?;
        if observed == *after {
            file.sync_all()?;
            return Ok(false);
        }
        // Linux chown preserves directory mode bits. A crash between chown
        // and chmod can leave the new owner with the original permissions.
        let intermediate = Node::Dir {
            uid,
            gid,
            mode: old_mode,
        };
        ensure!(
            observed == *before || observed == intermediate,
            "unexpected state at {path}; pending transaction retained"
        );
        Self::set_directory_metadata(&file, after)?;
        ensure!(
            directory_metadata(&file)? == *after,
            "could not restore directory metadata at {path}; pending transaction retained"
        );
        Ok(true)
    }
}

fn child_paths(directory: &File, prefix: &str) -> Result<Vec<String>> {
    let mut children = Vec::new();
    for entry in fs::read_dir(Root::fd_path(directory))? {
        let name = entry?.file_name();
        let name = name
            .to_str()
            .with_context(|| format!("non-UTF-8 name {name:?} in {prefix:?}"))?;
        children.push(format!("{prefix}{name}"));
    }
    children.sort_unstable_by(|a, b| b.cmp(a));
    Ok(children)
}

fn directory_metadata(file: &File) -> Result<Node> {
    let info = fstat(file)?;
    Ok(Node::Dir {
        uid: info.st_uid,
        gid: info.st_gid,
        mode: info.st_mode & 0o7777,
    })
}

/// Copy a verified staged payload into a new inode. A reflink shares the
/// verified extents without reading them; otherwise std::io::copy uses
/// copy_file_range, so the data stays in the kernel. Neither re-hashes the
/// copy: the source was hashed after staging, and the destination is a
/// private inode in the image's staging directory until it is renamed.
fn copy_verified(source: &mut File, destination: &mut File) -> Result<()> {
    let size = source.metadata()?.len();
    if size > 0 && ioctl_ficlone(&*destination, &*source).is_ok() {
        return Ok(());
    }
    destination.set_len(0)?;
    destination.rewind()?;
    source.rewind()?;
    let copied = std::io::copy(source, destination)?;
    ensure!(copied == size, "staged payload changed while copying");
    Ok(())
}

fn file_node(file: &File) -> Result<Node> {
    let metadata = file.metadata()?;
    Ok(Node::File {
        sha256: digest_reader(file)?,
        size: metadata.len(),
        mode: metadata.mode() & 0o7777,
        uid: metadata.uid(),
        gid: metadata.gid(),
        capabilities: FileCapabilities::read(file)?,
    })
}

fn xattr_names(file: &File) -> Result<Vec<OsString>> {
    let mut names = vec![0; XATTR_LIMIT];
    let len = match flistxattr(file, &mut names[..]) {
        Ok(len) => len,
        Err(Errno::NOTSUP) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(names[..len]
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| OsStr::from_bytes(name).to_owned())
        .collect())
}

// Linux limits both xattr lists and individual values to 64 KiB.
const XATTR_LIMIT: usize = 64 * 1024;

fn copy_config_xattrs(source: &File, destination: &File) -> Result<()> {
    let names = xattr_names(source)?;
    ensure!(
        !names.iter().any(|name| name == ATTRIBUTE),
        "unexpected file capabilities on preserved configuration"
    );
    for name in xattr_names(destination)? {
        if !names.contains(&name) {
            fremovexattr(destination, &name)?;
        }
    }
    if names.is_empty() {
        return Ok(());
    }
    let mut value = vec![0; XATTR_LIMIT];
    let mut inherited = vec![0; XATTR_LIMIT];
    for name in names {
        let len = fgetxattr(source, &name, &mut value[..])?;
        let equal = fgetxattr(destination, &name, &mut inherited[..])
            .is_ok_and(|existing| existing == len && inherited[..existing] == value[..len]);
        if !equal {
            fsetxattr(destination, &name, &value[..len], XattrFlags::empty())?;
        }
    }
    Ok(())
}

fn set_metadata(file: &File, mode: u32, uid: u32, gid: u32) -> Result<()> {
    let info = fstat(file)?;
    if (info.st_uid, info.st_gid) != (uid, gid) {
        fchown(file, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))?;
    }
    fchmod(file, Mode::from_raw_mode(mode))?;
    let info = fstat(file)?;
    ensure!(
        (info.st_uid, info.st_gid, info.st_mode & 0o7777) == (uid, gid, mode),
        "filesystem did not retain the requested ownership and mode"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn batches_directory_syncs_and_survives_removed_directories() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = Root::open(temp.path())?;
        fs::create_dir_all(temp.path().join("a/b"))?;
        fs::create_dir(temp.path().join("stage"))?;
        let staging = File::open(temp.path().join("stage"))?;
        let mut syncs = DirSyncs::default();
        let metadata = fs::metadata(temp.path())?;
        let node = Node::File {
            sha256: crate::model::digest(b"data"),
            size: 4,
            mode: 0o644,
            uid: metadata.uid(),
            gid: metadata.gid(),
            capabilities: None,
        };
        fs::write(temp.path().join("stage/payload"), "data")?;
        for path in ["a/b/one", "a/b/two", "a/three"] {
            let payload = File::open(temp.path().join("stage/payload"))?;
            root.replace(
                path,
                &node,
                Some(ReplacementSource::Staged(payload)),
                &staging,
                false,
                &mut syncs,
            )?;
        }
        assert_eq!(
            syncs
                .directories
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["a", "a/b"]
        );
        assert!(syncs.staging);
        for path in ["a/b/one", "a/b/two"] {
            root.remove_file_deferred(path, &mut syncs)?;
        }
        assert!(root.remove_empty_dir("a/b", &mut syncs)?);
        // The removed directory falls back to its surviving parent.
        root.sync_dirs(&mut syncs, Some(&staging))?;
        assert!(syncs.is_empty());
        assert_eq!(fs::read_to_string(temp.path().join("a/three"))?, "data");
        assert_eq!(root.inspect("a/three")?, Some(node));
        Ok(())
    }

    #[test]
    fn fingerprints_follow_changes_and_settle_after_the_margin() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = Root::open(temp.path())?;
        fs::write(temp.path().join("file"), "data")?;
        // A file changed moments ago is hashed but not recorded.
        let fresh = root.inspect_with("file", None)?;
        assert!(fresh.observed.is_none() && !fresh.trusted);
        TEST_SETTLED_NS.set(20_000_000);
        std::thread::sleep(std::time::Duration::from_millis(40));
        let observed = root.inspect_with("file", None)?.observed.unwrap();
        assert_eq!(observed.sha256, crate::model::digest(b"data"));
        let mut forged = observed.clone();
        forged.sha256 = crate::model::digest(b"other");
        // A matching fingerprint stands in for the contents.
        let trusted = root.inspect_with("file", Some(&forged))?;
        assert!(trusted.trusted && trusted.observed.is_none());
        assert_eq!(trusted.node, Some(forged.node()));
        assert!(root.file_matching("file", &observed.fingerprint).is_ok());
        fs::write(temp.path().join("file"), "DATA")?;
        let changed = root.inspect_with("file", Some(&forged))?;
        assert!(!changed.trusted);
        assert!(matches!(
            changed.node,
            Some(Node::File { ref sha256, .. }) if *sha256 == crate::model::digest(b"DATA")
        ));
        assert!(root.file_matching("file", &observed.fingerprint).is_err());
        Ok(())
    }

    #[test]
    fn resumes_directory_repair_after_ownership_change() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = Root::open(temp.path())?;
        let path = temp.path().join("dir");
        fs::create_dir(&path)?;
        let metadata = fs::metadata(&path)?;
        let (uid, gid) = (metadata.uid(), metadata.gid());
        for mode in [0o700, 0o2770, 0o3770] {
            let before = Node::Dir {
                uid: uid ^ 1,
                gid,
                mode,
            };
            let after = Node::Dir {
                uid,
                gid,
                mode: 0o1777,
            };
            fs::set_permissions(&path, fs::Permissions::from_mode(0o770))?;
            assert!(root.repair_directory("dir", &before, &after).is_err());
            fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
            assert!(root.repair_directory("dir", &before, &after)?);
            assert_eq!(root.inspect("dir")?, Some(after.clone()));
            assert!(!root.repair_directory("dir", &before, &after)?);
        }
        Ok(())
    }
}
