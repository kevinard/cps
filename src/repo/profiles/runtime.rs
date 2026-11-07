use std::{
    collections::{BTreeMap, VecDeque, btree_map::Entry},
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use object::elf::{EM_AARCH64, EM_X86_64};

use crate::{
    image::{Installed, Managed, root::Root},
    kdl_format::publish_policy::Profile,
    model::{Dependency, Node},
};

use super::elf::{self, Elf};

const PATH_LIMIT: usize = 4096;
pub(crate) const DEFAULT_LIBRARY_DIRS: [&str; 4] = ["/usr/lib", "/lib", "/usr/lib64", "/lib64"];

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
    let machine = machine(&profile.arch)?;
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
    let elf_at = |path: &str| files.get(path).and_then(|key| payloads[key].as_ref());
    check_paths(
        paths,
        machine,
        &profile.elf_library_dirs,
        elf_at,
        |source, provider| {
            if profile.elf_require_declarations {
                check_declared_provider(installed, paths, source, provider)?;
            }
            Ok(())
        },
    )
}

pub(crate) fn audit_installed(
    root: &Root,
    paths: &BTreeMap<String, Managed>,
    arch: &str,
    library_dirs: &[String],
) -> Result<()> {
    let mut files = BTreeMap::new();
    for (path, managed) in paths {
        if let Node::File { sha256, size, .. } = &managed.node
            && !managed.preserve
        {
            let inspected = elf::inspect(root.file(path)?, *size, sha256)
                .with_context(|| format!("inspecting installed file {path}"))?;
            if let Some(inspected) = inspected {
                files.insert(path.as_str(), inspected);
            }
        }
    }
    check_paths(
        paths,
        machine(arch)?,
        library_dirs,
        |path| files.get(path),
        |_, _| Ok(()),
    )
}

fn check_paths<'a>(
    paths: &BTreeMap<String, Managed>,
    machine: u16,
    library_dirs: &[String],
    elf_at: impl Fn(&str) -> Option<&'a Elf>,
    dependency: impl Fn(&str, &str) -> Result<()>,
) -> Result<()> {
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
                    compatible_interpreter(&paths[target].node, elf_at(target), machine),
                    "incompatible ELF interpreter {interpreter}"
                );
                dependency(path, target)?;
            }
            let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            let origin = format!("/{dir}");
            let search = library_search(info, &origin, library_dirs)?;
            for needed in &info.needed {
                let (target, _) = find_library(paths, needed, &origin, &search)?
                    .with_context(|| format!("missing ELF dependency {needed}"))?;
                check_library(info, machine, needed, target, elf_at(target))?;
                dependency(path, target)?;
            }
            Ok(())
        };
        check().with_context(|| format!("checking ELF runtime for {path}"))?;
    }
    Ok(())
}

fn check_declared_provider(
    installed: &BTreeMap<String, Installed>,
    paths: &BTreeMap<String, Managed>,
    source: &str,
    target: &str,
) -> Result<()> {
    let consumer = paths[source]
        .owners
        .first()
        .context("missing ELF consumer owner")?;
    let provider = paths[target]
        .owners
        .first()
        .context("missing ELF provider owner")?;
    if consumer == provider {
        return Ok(());
    }
    let consumer = &installed[consumer].published.manifest;
    let provider = &installed[provider].published.manifest;
    for dependency in &consumer.dependency {
        if let Dependency::Require(requirement) = dependency
            && requirement.package == provider.name
            && requirement.matches(&provider.version)?
        {
            return Ok(());
        }
    }
    anyhow::bail!(
        "{} needs a direct require for {} via {target}",
        consumer.id(),
        provider.id()
    )
}

pub(crate) fn find_library<'a>(
    paths: &'a BTreeMap<String, Managed>,
    needed: &str,
    origin: &str,
    search: &[String],
) -> Result<Option<(&'a str, String)>> {
    ensure!(!needed.is_empty(), "empty ELF dependency");
    if needed.contains(['/', '$']) {
        let path = expand_absolute(needed, origin)?;
        return Ok(resolve(paths, &path)?.map(|target| (target, path)));
    }
    for dir in search {
        let path = format!("{}/{needed}", dir.trim_end_matches('/'));
        if let Some(target) = resolve(paths, &path)? {
            return Ok(Some((target, path)));
        }
    }
    Ok(None)
}

pub(crate) fn check_library(
    consumer: &Elf,
    machine: u16,
    needed: &str,
    target: &str,
    library: Option<&Elf>,
) -> Result<()> {
    let library = library
        .filter(|library| native(library, machine) && library.shared_object)
        .with_context(|| format!("incompatible ELF dependency {needed} at {target}"))?;
    if let Some(versions) = consumer.required_versions.get(needed) {
        for (version, hash) in versions {
            ensure!(
                library.defined_versions.get(version) == Some(hash),
                "ELF dependency {needed} at {target} does not provide required version {version}"
            );
        }
    }
    Ok(())
}

pub(crate) fn machine(arch: &str) -> Result<u16> {
    match arch {
        "x86_64" => Ok(EM_X86_64),
        "aarch64" => Ok(EM_AARCH64),
        _ => bail!("unsupported architecture for ELF checking {arch}"),
    }
}

pub(crate) fn native(info: &Elf, machine: u16) -> bool {
    info.native_64_le && info.machine == machine
}

pub(crate) fn library_search(info: &Elf, origin: &str, defaults: &[String]) -> Result<Vec<String>> {
    let mut search = Vec::new();
    if let Some(runpath) = &info.runpath {
        for dir in runpath.split(':') {
            ensure!(search.len() < 64, "too many ELF runpath directories");
            search.push(expand_absolute(dir, origin)?);
        }
    }
    if info.use_default_dirs {
        search.extend(defaults.iter().cloned());
    }
    Ok(search)
}

pub(crate) fn expand_absolute(path: &str, origin: &str) -> Result<String> {
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

pub(crate) fn compatible_interpreter(node: &Node, elf: Option<&Elf>, machine: u16) -> bool {
    matches!(node, Node::File { mode, .. } if mode & 0o111 != 0)
        && elf.is_some_and(|loader| native(loader, machine) && loader.interpreter.is_none())
}

/// Resolve within the signed path map. Absolute symlink targets start at
/// the image root, including the compatibility links in a merged /usr.
pub(crate) fn resolve<'a>(
    paths: &'a BTreeMap<String, Managed>,
    absolute: &str,
) -> Result<Option<&'a str>> {
    walk_path(absolute, |path| {
        Ok(paths.get_key_value(path).and_then(|(name, entry)| {
            let kind = match &entry.node {
                Node::Dir { .. } => PathKind::Directory,
                Node::File { .. } => PathKind::File(name),
                Node::Hardlink { target } => PathKind::File(target),
                Node::Symlink { target, .. } => PathKind::Symlink(target),
                Node::Special => return None,
            };
            Some(kind)
        }))
    })
}

pub(crate) enum PathKind<'a> {
    Directory,
    File(&'a str),
    Symlink(&'a str),
}

pub(crate) fn walk_path<'a>(
    absolute: &str,
    mut lookup: impl FnMut(&str) -> Result<Option<PathKind<'a>>>,
) -> Result<Option<&'a str>> {
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
        let Some(kind) = lookup(&path)? else {
            return Ok(None);
        };
        match kind {
            PathKind::Symlink(target) => {
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
            PathKind::Directory => {}
            PathKind::File(path) if pending.is_empty() => return Ok(Some(path)),
            _ => return Ok(None),
        }
    }
    Ok(None)
}
