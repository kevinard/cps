use std::{fs::File, path::Path};

use anyhow::{Context, Result, bail, ensure};
use rustix::{
    fs::{FileType, listxattr, readlinkat},
    io::Errno,
};

use crate::{
    image::root::Root,
    kdl_format::manifest_text,
    model::{Action, FORMAT, Manifest, validate_name, validate_publisher},
    repo::MANIFEST_LIMIT,
};

/// Generate an unsigned manifest for review before publishing.
pub fn generate_manifest(
    proto: &Path,
    publisher: &str,
    name: &str,
    version: &str,
    uid: u32,
    gid: u32,
) -> Result<String> {
    validate_name(name)?;
    validate_publisher(publisher)?;
    ensure!(uid != u32::MAX && gid != u32::MAX, "invalid owner");
    let mut manifest = Manifest {
        format: FORMAT,
        publisher: publisher.to_owned(),
        name: name.to_owned(),
        version: version.parse()?,
        dependency: Vec::new(),
        action: Vec::new(),
        systemd: Vec::new(),
        kernel: None,
    };
    let root = Root::open(proto)?;
    root.walk(None, |path, entry, info| {
        let kind = FileType::from_raw_mode(info.st_mode);
        let mode = || {
            let mode = info.st_mode & 0o7777;
            ensure!(
                mode <= 0o777,
                "unsupported special permission bits at {path:?}"
            );
            check_xattrs(entry, path)?;
            Ok::<_, anyhow::Error>(format!("{mode:04o}"))
        };
        let action = match kind {
            FileType::Directory => Action::Dir {
                path: path.to_owned(),
                mode: mode()?,
                uid,
                gid,
                facet: None,
                arch: None,
            },
            FileType::RegularFile => {
                ensure!(
                    info.st_nlink == 1,
                    "unsupported hard-linked file at {path:?}"
                );
                Action::File {
                    path: path.to_owned(),
                    mode: mode()?,
                    uid,
                    gid,
                    preserve: false,
                    facet: None,
                    arch: None,
                    sha256: None,
                    size: None,
                }
            }
            FileType::Symlink => Action::Symlink {
                path: path.to_owned(),
                target: readlinkat(entry, "", Vec::new())?
                    .into_string()
                    .with_context(|| format!("non-UTF-8 symlink target at {path:?}"))?,
                uid,
                gid,
                facet: None,
                arch: None,
            },
            _ => bail!("unsupported filesystem object at {path:?}"),
        };
        manifest.action.push(action);
        Ok(true)
    })?;
    manifest.validate(false)?;
    let text = manifest_text(&manifest);
    ensure!(
        text.len() as u64 <= MANIFEST_LIMIT,
        "manifest exceeds {MANIFEST_LIMIT} bytes"
    );
    Ok(text)
}

fn check_xattrs(entry: &File, path: &str) -> Result<()> {
    // An empty buffer queries the size without allocating or reading values.
    let empty: &mut [u8] = &mut [];
    match listxattr(Root::fd_path(entry), empty) {
        Ok(0) | Err(Errno::NOTSUP) => Ok(()),
        Ok(_) => bail!("unsupported extended attributes at {path:?}"),
        Err(error) => Err(error).with_context(|| format!("listing attributes at {path:?}")),
    }
}
