use std::{collections::BTreeMap, fs::File, os::unix::ffi::OsStrExt};

use anyhow::{Context, Result, bail};
use rustix::{
    fs::{FileType, Stat, fstat, getxattr, listxattr, readlinkat},
    io::Errno,
};
use sha2::{Digest, Sha256};

use crate::{image::root::Root, model::digest_reader};

/// Fingerprint the staged root independently of inode numbers and timestamps.
pub(super) fn digest(root: &Root) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(b"cps-export-tree-v1\0");
    let mut links = BTreeMap::<_, String>::new();
    // Linux caps both the list of names and each individual value at 64 KiB.
    let mut names = vec![0; 64 * 1024];
    let mut value = vec![0; 64 * 1024];

    let mut add = |path: &str, entry: &File, info: &Stat| -> Result<bool> {
        let kind = FileType::from_raw_mode(info.st_mode);
        field(&mut hash, path.as_bytes());
        hash.update(info.st_mode.to_le_bytes());
        hash.update(info.st_uid.to_le_bytes());
        hash.update(info.st_gid.to_le_bytes());
        if kind != FileType::Directory && info.st_nlink > 1 {
            let identity = (info.st_dev, info.st_ino);
            if let Some(first) = links.get(&identity) {
                hash.update([1]);
                field(&mut hash, first.as_bytes());
                return Ok(true);
            }
            links.insert(identity, path.to_owned());
        }
        hash.update([0]);
        let pinned = Root::fd_path(entry);
        match kind {
            FileType::RegularFile => {
                hash.update(digest_reader(File::open(&pinned)?)?.as_bytes());
            }
            FileType::Symlink => {
                field(&mut hash, readlinkat(entry, "", Vec::new())?.as_bytes());
            }
            FileType::Directory => {}
            _ => bail!("cannot fingerprint special file {path:?}"),
        }
        // An O_PATH descriptor pins the actual symlink as well as files and directories.
        let len = match listxattr(&pinned, &mut names[..]) {
            Ok(len) => len,
            Err(Errno::NOTSUP) => 0,
            Err(error) => {
                return Err(error).with_context(|| format!("listing attributes of {path:?}"));
            }
        };
        let mut attrs: Vec<_> = names[..len]
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .collect();
        attrs.sort_unstable();
        hash.update((attrs.len() as u64).to_le_bytes());
        for name in attrs {
            let len = getxattr(&pinned, std::ffi::OsStr::from_bytes(name), &mut value[..])
                .with_context(|| format!("reading attributes of {path:?}"))?;
            field(&mut hash, name);
            field(&mut hash, &value[..len]);
        }
        Ok(true)
    };
    add("", root.as_file(), &fstat(root.as_file())?)?;
    root.walk(None, add)?;
    Ok(hex::encode(hash.finalize()))
}

fn field(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value);
}
