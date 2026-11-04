use std::{
    collections::{BTreeMap, VecDeque, btree_map::Entry},
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use object::elf::{EM_AARCH64, EM_X86_64};

use crate::{
    image::{Installed, Managed, root::Root},
    kdl_format::publish_policy::Profile,
    model::Node,
};

use super::elf::{self, Elf};

const PATH_LIMIT: usize = 4096;

pub(super) fn check(
    repo: &Path,
    publisher: &str,
    profile: &Profile,
    installed: &BTreeMap<String, Installed>,
    paths: &BTreeMap<String, Managed>,
) -> Result<()> {
    if profile.elf_library_dirs.is_empty() {
        return Ok(());
    }
    let machine = match profile.arch.as_str() {
        "x86_64" => EM_X86_64,
        "aarch64" => EM_AARCH64,
        arch => bail!("unsupported architecture for ELF checking {arch}"),
    };
    let mut repositories = BTreeMap::from([(publisher, Root::open(repo)?)]);
    for companion in &profile.companions {
        repositories.insert(
            &companion.config.name,
            Root::open(&companion.config.repository)?,
        );
    }
    let mut payloads = BTreeMap::new();
    let mut files = BTreeMap::new();
    for (path, managed) in paths {
        let Node::File { sha256, size, .. } = &managed.node else {
            continue;
        };
        let owner = managed
            .owners
            .first()
            .context("ELF check found an unowned file")?;
        let publisher = installed[owner].published.manifest.publisher.as_str();
        let key = (publisher, sha256.as_str());
        if let Entry::Vacant(entry) = payloads.entry(key) {
            let inspected = (|| {
                let file = repositories[publisher].file(&format!("blobs/{sha256}"))?;
                elf::inspect(file, *size, sha256)
            })()
            .with_context(|| format!("inspecting {path} from {publisher}"))?;
            entry.insert(inspected);
        }
        files.insert(path.as_str(), key);
    }
    let elf_at = |path| files.get(path).and_then(|key| payloads[key].as_ref());
    for (path, managed) in paths {
        let source = match &managed.node {
            Node::File { .. } => path,
            Node::Hardlink { target } => target,
            _ => continue,
        };
        let Some(info) = elf_at(source.as_str()) else {
            continue;
        };
        if info.interpreter.is_none() && info.needed.is_empty() {
            continue;
        }
        let check = || {
            ensure!(
                native(info, machine),
                "ELF does not match the profile architecture"
            );
            if let Some(interpreter) = &info.interpreter {
                let target = resolve(paths, interpreter)?
                    .with_context(|| format!("missing ELF interpreter {interpreter}"))?;
                ensure!(
                    elf_at(target).is_some_and(
                        |loader| native(loader, machine) && loader.interpreter.is_none()
                    ) && matches!(paths[target].node, Node::File { mode, .. } if mode & 0o111 != 0),
                    "incompatible ELF interpreter {interpreter}"
                );
            }
            let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            let origin = format!("/{dir}");
            let mut search = Vec::new();
            if let Some(runpath) = &info.runpath {
                for dir in runpath.split(':') {
                    ensure!(search.len() < 64, "too many ELF runpath directories");
                    search.push(expand_absolute(dir, &origin)?);
                }
            }
            if info.use_default_dirs {
                search.extend(profile.elf_library_dirs.iter().cloned());
            }
            for needed in &info.needed {
                ensure!(!needed.is_empty(), "empty ELF dependency");
                let target = if needed.contains('/') || needed.contains('$') {
                    resolve(paths, &expand_absolute(needed, &origin)?)?
                } else {
                    let mut found = None;
                    for dir in &search {
                        let path = format!("{}/{needed}", dir.trim_end_matches('/'));
                        if let Some(target) = resolve(paths, &path)? {
                            found = Some(target);
                            break;
                        }
                    }
                    found
                };
                let target = target.with_context(|| format!("missing ELF dependency {needed}"))?;
                ensure!(
                    elf_at(target)
                        .is_some_and(|library| native(library, machine) && library.shared_object),
                    "incompatible ELF dependency {needed} at {target}"
                );
            }
            Ok(())
        };
        check().with_context(|| format!("checking ELF runtime for {path}"))?;
    }
    Ok(())
}

fn native(info: &Elf, machine: u16) -> bool {
    info.native_64_le && info.machine == machine
}

fn expand_absolute(path: &str, origin: &str) -> Result<String> {
    let mut remaining = path;
    let mut expanded = String::new();
    while let Some((before, token)) = remaining.split_once('$') {
        expanded.push_str(before);
        remaining = token
            .strip_prefix("{ORIGIN}")
            .or_else(|| {
                token.strip_prefix("ORIGIN").filter(|suffix| {
                    !suffix.starts_with(|ch: char| ch.is_ascii_alphanumeric() || ch == '_')
                })
            })
            .with_context(|| format!("unsupported dynamic token in {path:?}"))?;
        ensure!(
            expanded.len().saturating_add(origin.len()) < PATH_LIMIT,
            "expanded ELF path is too long"
        );
        expanded.push_str(origin);
    }
    expanded.push_str(remaining);
    ensure!(
        expanded.starts_with('/'),
        "ELF path must be absolute or relative to $ORIGIN: {path:?}"
    );
    Ok(expanded)
}

/// Resolve within the signed path map. Absolute symlink targets start at
/// the image root, including the compatibility links in a merged /usr.
fn resolve<'a>(paths: &'a BTreeMap<String, Managed>, absolute: &str) -> Result<Option<&'a str>> {
    ensure!(absolute.len() < PATH_LIMIT, "ELF path is too long");
    ensure!(
        absolute.starts_with('/'),
        "ELF path must be absolute: {absolute:?}"
    );
    let mut pending: VecDeque<_> = absolute
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if absolute.ends_with('/') {
        pending.push_back(".");
    }
    let mut resolved = Vec::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        match part {
            "." => continue,
            ".." => {
                resolved.pop();
                continue;
            }
            _ => resolved.push(part),
        }
        let path = resolved.join("/");
        let Some((name, file)) = paths.get_key_value(&path) else {
            return Ok(None);
        };
        match &file.node {
            Node::Symlink { target, .. } => {
                links += 1;
                ensure!(links <= 40, "too many symlinks resolving {absolute:?}");
                ensure!(target.len() < PATH_LIMIT, "ELF symlink target is too long");
                resolved.pop();
                if target.starts_with('/') {
                    resolved.clear();
                }
                // A trailing slash requires the resolved target to be a directory.
                if target.ends_with('/') {
                    pending.push_front(".");
                }
                for part in target.rsplit('/').filter(|part| !part.is_empty()) {
                    pending.push_front(part);
                }
            }
            Node::Dir { .. } => {}
            Node::Hardlink { target } if pending.is_empty() => return Ok(Some(target)),
            Node::File { .. } if pending.is_empty() => return Ok(Some(name)),
            _ => return Ok(None),
        }
    }
    Ok(None)
}
