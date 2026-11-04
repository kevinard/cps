use std::{
    fs::{self, File},
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{
        AtFlags, FileType, Mode, OFlags, ResolveFlags, Stat, chownat, fchmod, fchown, fstat,
        listxattr, mkdirat, open, openat2, readlinkat, renameat, statat, symlinkat, unlinkat,
    },
    io::Errno,
    process::{Gid, Uid},
};

use crate::model::{Node, digest_reader, validate_path};

pub struct Root(File);

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

    pub(crate) fn has_xattrs(entry: &File) -> Result<bool> {
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
            FileType::RegularFile => {
                let file = self.file(path)?;
                Node::File {
                    sha256: digest_reader(file)?,
                    size: info.st_size.try_into()?,
                    uid,
                    gid,
                    mode,
                }
            }
            _ => Node::Special,
        }))
    }

    pub fn replace(
        &self,
        path: &str,
        node: &Node,
        payload: Option<File>,
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
            Node::File { mode, uid, gid, .. } => {
                let temp = openat2(
                    staging,
                    TEMP,
                    OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                    ResolveFlags::NO_SYMLINKS,
                )?;
                let mut temp = File::from(temp);
                std::io::copy(&mut payload.context("missing staged payload")?, &mut temp)?;
                temp.flush()?;
                set_metadata(&temp, *mode, *uid, *gid)?;
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
                set_metadata(&File::from(temp), *mode, *uid, *gid)?;
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
            Node::Special => anyhow::bail!("cannot materialize special file {path}"),
        }
        renameat(staging, TEMP, &parent, leaf)?;
        parent.sync_all()?;
        staging.sync_all()?;
        Ok(())
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        validate_path(path)?;
        let (parent, leaf) = self.parent(path)?;
        unlinkat(&parent, leaf, AtFlags::empty())?;
        parent.sync_all()?;
        Ok(())
    }

    pub fn remove_empty_dir(&self, path: &str) -> Result<bool> {
        validate_path(path)?;
        let entry = match self.open_relative(path, OFlags::PATH | OFlags::DIRECTORY) {
            Ok(entry) => entry,
            Err(Errno::XDEV) => return Ok(false),
            Err(error) => return Err(error).with_context(|| format!("opening directory {path}")),
        };
        if Self::has_xattrs(&entry)? {
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

fn set_metadata(file: &File, mode: u32, uid: u32, gid: u32) -> Result<()> {
    let info = fstat(file)?;
    if (info.st_uid, info.st_gid) != (uid, gid) {
        fchown(file, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))?;
    }
    fchmod(file, Mode::from_raw_mode(mode))?;
    file.sync_all()?;
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
