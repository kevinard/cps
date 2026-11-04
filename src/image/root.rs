use std::{
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
        AtFlags, CWD, FileType, Mode, OFlags, ResolveFlags, Stat, XattrFlags, chownat, fchmod,
        fchown, fgetxattr, flistxattr, fremovexattr, fsetxattr, fstat, linkat, listxattr, mkdirat,
        open, openat2, readlinkat, renameat, statat, symlinkat, unlinkat,
    },
    io::Errno,
    process::{Gid, Uid},
};

use crate::{
    file_caps::{ATTRIBUTE, FileCapabilities},
    model::{Node, copy_payload, digest_reader, validate_path},
};

pub struct Root(File);

pub(crate) enum ReplacementSource<'a> {
    Staged(File),
    LocalConfig(&'a Node),
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

    /// Visit each entry without following symlinks or crossing a mount.
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

    fn regular_entry(&self, path: &str) -> Result<Option<File>> {
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
        let (parent, leaf) = match self.parent(path) {
            Ok(entry) => entry,
            Err(Errno::NOENT) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let info = match statat(&parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(info) => info,
            Err(Errno::NOENT) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let (uid, gid, mode) = (info.st_uid, info.st_gid, info.st_mode & 0o7777);
        Ok(Some(match FileType::from_raw_mode(info.st_mode) {
            FileType::Directory => Node::Dir { uid, gid, mode },
            FileType::Symlink => {
                let target = readlinkat(&parent, leaf, Vec::new())?.into_string()?;
                Node::Symlink { target, uid, gid }
            }
            FileType::RegularFile => file_node(&self.file(path)?)?,
            _ => Node::Special,
        }))
    }

    pub fn replace(
        &self,
        path: &str,
        node: &Node,
        payload: Option<ReplacementSource<'_>>,
        staging: &File,
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
                    ReplacementSource::Staged(mut file) => {
                        std::io::copy(&mut file, &mut temp)?;
                    }
                    ReplacementSource::LocalConfig(before) => {
                        self.copy_local_config(path, before, node, &mut temp)?;
                    }
                }
                temp.flush()?;
                Self::set_file_metadata(&temp, node)
                    .with_context(|| format!("preparing file metadata for {path}"))?;
            }
            Node::Dir { mode, uid, gid } => {
                mkdirat(staging, TEMP, Mode::RWXU)?;
                let temp = openat2(
                    staging,
                    TEMP,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                    ResolveFlags::NO_SYMLINKS,
                )?;
                let temp = File::from(temp);
                set_metadata(&temp, *mode, *uid, *gid)?;
                temp.sync_all()?;
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
        renameat(staging, TEMP, &parent, leaf)?;
        parent.sync_all()?;
        staging.sync_all()?;
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
        validate_path(path)?;
        let (parent, leaf) = self.parent(path)?;
        unlinkat(&parent, leaf, AtFlags::empty())?;
        parent.sync_all()?;
        Ok(())
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

    pub fn remove_empty_dir(&self, path: &str) -> Result<bool> {
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
                parent.sync_all()?;
                Ok(true)
            }
            Err(Errno::NOTEMPTY | Errno::EXIST | Errno::BUSY) => Ok(false),
            Err(error) => Err(error).with_context(|| format!("pruning directory {path}")),
        }
    }

    pub fn repair_directory(&self, path: &str, before: &Node, after: &Node) -> Result<bool> {
        let Node::Dir { mode: old_mode, .. } = *before else {
            anyhow::bail!("expected directory metadata at {path}");
        };
        let Node::Dir { uid, gid, mode } = *after else {
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
        set_metadata(&file, mode, uid, gid)?;
        file.sync_all()?;
        ensure!(
            directory_metadata(&file)? == *after,
            "could not restore directory metadata at {path}; pending transaction retained"
        );
        Ok(true)
    }

    pub fn sync_parent(&self, mut path: &str) -> Result<()> {
        loop {
            match self.parent(path) {
                Ok((parent, _)) => return Ok(parent.sync_all()?),
                // During replay, deeper removals may already have completed
                // along with their parent. Sync the nearest surviving ancestor.
                Err(Errno::NOENT) => {
                    path = path.rsplit_once('/').context("missing image parent")?.0
                }
                Err(error) => return Err(error.into()),
            }
        }
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn resumes_directory_repair_after_ownership_change() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = Root::open(temp.path())?;
        let path = temp.path().join("dir");
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o770))?;
        let metadata = fs::metadata(&path)?;
        let (uid, gid) = (metadata.uid(), metadata.gid());
        let before = Node::Dir {
            uid: uid ^ 1,
            gid,
            mode: 0o700,
        };
        let after = Node::Dir {
            uid,
            gid,
            mode: 0o755,
        };
        assert!(root.repair_directory("dir", &before, &after).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        assert!(root.repair_directory("dir", &before, &after)?);
        assert_eq!(root.inspect("dir")?, Some(after.clone()));
        assert!(!root.repair_directory("dir", &before, &after)?);
        Ok(())
    }
}
