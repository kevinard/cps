use std::{
    collections::{BTreeMap, btree_map::Entry},
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{FileType, readlinkat};

use crate::{
    file_caps::FileCapabilities,
    image::root::Root,
    kdl_format::manifest_text,
    model::{Action, FORMAT, Manifest, validate_name, validate_publisher},
    repo::MANIFEST_LIMIT,
};

struct LinkGroup {
    target: String,
    links: usize,
    seen: usize,
}

/// Generate an unsigned manifest for review before publishing.
pub fn generate_manifest(
    proto: &Path,
    publisher: &str,
    name: &str,
    version: &str,
    uid: u32,
    gid: u32,
) -> Result<String> {
    generate_manifest_for_architecture(proto, publisher, name, version, uid, gid, None)
}

/// Generate an unsigned manifest for a portable or architecture-specific package.
pub fn generate_manifest_for_architecture(
    proto: &Path,
    publisher: &str,
    name: &str,
    version: &str,
    uid: u32,
    gid: u32,
    architecture: Option<&str>,
) -> Result<String> {
    validate_name(name)?;
    validate_publisher(publisher)?;
    ensure!(uid != u32::MAX && gid != u32::MAX, "invalid owner");
    let mut manifest = Manifest {
        format: FORMAT,
        publisher: publisher.to_owned(),
        name: name.to_owned(),
        version: version.parse()?,
        architecture: architecture.map(str::to_owned),
        dependency: Vec::new(),
        action: Vec::new(),
        systemd: Vec::new(),
        kernel: None,
    };
    let mut groups = BTreeMap::new();
    let root = Root::open(proto)?;
    root.walk(None, |path, entry, info| {
        let kind = FileType::from_raw_mode(info.st_mode);
        let mode = |allow_capability| {
            let mode = info.st_mode & 0o7777;
            ensure!(
                !Root::has_xattrs(entry, allow_capability)
                    .with_context(|| format!("listing attributes at {path:?}"))?,
                "unsupported extended attributes at {path:?}"
            );
            Ok::<_, anyhow::Error>(format!("{mode:04o}"))
        };
        let action = match kind {
            FileType::Directory => Action::Dir {
                path: path.to_owned(),
                mode: mode(false)?,
                uid,
                gid,
                facet: None,
                arch: None,
            },
            FileType::RegularFile => {
                if info.st_nlink > 1 {
                    let links = info.st_nlink as usize;
                    match groups.entry((info.st_dev, info.st_ino)) {
                        Entry::Vacant(group) => {
                            group.insert(LinkGroup {
                                target: path.to_owned(),
                                links,
                                seen: 1,
                            });
                        }
                        Entry::Occupied(mut group) => {
                            let group = group.get_mut();
                            ensure!(
                                group.links == links,
                                "hard link group changed while scanning {path:?}"
                            );
                            group.seen += 1;
                            manifest.action.push(Action::Hardlink {
                                path: path.to_owned(),
                                target: group.target.clone(),
                                facet: None,
                                arch: None,
                            });
                            return Ok(true);
                        }
                    }
                }
                let capabilities = FileCapabilities::read(entry)
                    .with_context(|| format!("reading file capabilities at {path:?}"))?;
                Action::File {
                    path: path.to_owned(),
                    mode: mode(capabilities.is_some())?,
                    uid,
                    gid,
                    preserve: false,
                    facet: None,
                    arch: None,
                    sha256: None,
                    size: None,
                    capabilities,
                    original_name: None,
                }
            }
            FileType::Symlink => {
                ensure!(
                    info.st_nlink == 1,
                    "unsupported hard-linked symlink at {path:?}"
                );
                Action::Symlink {
                    path: path.to_owned(),
                    target: readlinkat(entry, "", Vec::new())?
                        .into_string()
                        .with_context(|| format!("non-UTF-8 symlink target at {path:?}"))?,
                    uid,
                    gid,
                    facet: None,
                    arch: None,
                }
            }
            _ => bail!("unsupported filesystem object at {path:?}"),
        };
        manifest.action.push(action);
        Ok(true)
    })?;
    for group in groups.values() {
        ensure!(
            group.seen == group.links,
            "hard link group for {:?} extends outside the prototype or changed while scanning",
            group.target
        );
    }
    manifest.validate(false)?;
    let text = manifest_text(&manifest);
    ensure!(
        text.len() as u64 <= MANIFEST_LIMIT,
        "manifest exceeds {MANIFEST_LIMIT} bytes"
    );
    Ok(text)
}
