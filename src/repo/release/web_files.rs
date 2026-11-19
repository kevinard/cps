//! Files of a served web root, shared by deployments and replicas.
//!
//! Every file and directory a web server reads is made world-readable
//! explicitly, whatever the process umask, because systemd services and
//! signing accounts often run with `UMask=0077`.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::{fd::AsRawFd, unix::fs::PermissionsExt},
    path::Path,
};

use anyhow::Result;
use url::Url;

/// Create a new file that every web server can read.
pub(super) fn create_public(path: &Path) -> Result<File> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o644))?;
    Ok(file)
}

/// Create a directory that every web server can traverse.
pub(super) fn create_public_dir(path: &Path) -> Result<()> {
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Write a new public file and make it durable.
pub(super) fn write_public(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = create_public(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Copy a file into a new public file and make it durable.
pub(super) fn copy_public(source: &Path, destination: &Path) -> Result<()> {
    let mut output = create_public(destination)?;
    std::io::copy(&mut File::open(source)?, &mut output)?;
    output.sync_all()?;
    Ok(())
}

/// Reuse a served file by hard link, or copy it across filesystems.
pub(super) fn link_or_copy(source: &Path, destination: &Path) -> Result<()> {
    if fs::hard_link(source, destination).is_err() {
        copy_public(source, destination)?;
    }
    Ok(())
}

/// Compare two files without reading either into memory.
pub(super) fn same_contents(left: &Path, right: &Path) -> Result<bool> {
    use std::io::Read;
    let (mut left, mut right) = (File::open(left)?, File::open(right)?);
    if left.metadata()?.len() != right.metadata()?.len() {
        return Ok(false);
    }
    let (mut a, mut b) = (vec![0; 1 << 16], vec![0; 1 << 16]);
    loop {
        let count = left.read(&mut a)?;
        if count == 0 {
            return Ok(right.read(&mut b[..1])? == 0);
        }
        right.read_exact(&mut b[..count])?;
        if a[..count] != b[..count] {
            return Ok(false);
        }
    }
}

/// A `file:` URL for an open directory. The TUF file transport does not
/// decode URLs, so directories are named through their descriptor instead of
/// a path that may need escaping. Keep `directory` open while it is used.
pub(super) fn fd_url(directory: &File) -> Result<Url> {
    Ok(Url::parse(&format!(
        "file:///proc/self/fd/{}/",
        directory.as_raw_fd()
    ))?)
}
