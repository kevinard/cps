use std::{
    fs::{self, File},
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{
        AtFlags, FileType, Mode, OFlags, ResolveFlags, chownat, fchmod, fchown, fstat, mkdirat,
        open, openat2, readlinkat, renameat, statat, symlinkat, unlinkat,
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

    pub fn offline_path(&self) -> Result<PathBuf> {
        let image = self.0.metadata()?;
        let host = fs::metadata("/")?;
        ensure!(
            (image.dev(), image.ino()) != (host.dev(), host.ino()),
            "systemd actions require an inactive image root"
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

    pub fn sync_parent(&self, path: &str) -> Result<()> {
        self.parent(path)?.0.sync_all()?;
        Ok(())
    }
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
