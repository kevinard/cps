use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use rustix::fs::FileType;

use crate::{
    image::{Image, Managed, State, root::Root},
    model::{Node, PackageSpec, validate_path},
    plan::{check_dependencies, effective_paths, hardlink_groups},
    repo::profiles::runtime::{self, DEFAULT_LIBRARY_DIRS},
};

#[derive(Debug)]
pub struct Finding {
    pub is_error: bool,
    pub message: String,
}

pub fn verify(path: &Path) -> Result<Vec<Finding>> {
    verify_image(&Image::open_query(path)?)
}

/// Audit the entire installed managed ELF closure without executing its binaries.
pub fn verify_elf_runtime(path: &Path, library_dirs: &[String]) -> Result<()> {
    let image = Image::open_query(path)?;
    image.require_clean()?;
    let state = image.state()?;
    image.verify_catalog_archives(&state)?;
    let paths = manifest_paths(&state)?;
    ensure!(
        paths == state.paths,
        "path ownership index differs from installed manifests"
    );
    let defaults = DEFAULT_LIBRARY_DIRS.map(str::to_owned);
    let library_dirs = if library_dirs.is_empty() {
        &defaults[..]
    } else {
        library_dirs
    };
    crate::kdl_format::publish_policy::validate_elf_dirs(&state.settings.arch, library_dirs)?;
    runtime::audit_installed(&image.root, &paths, &state.settings.arch, library_dirs)
}

pub fn verify_selected(root: &Path, packages: &[String], paths: &[String]) -> Result<Vec<Finding>> {
    let image = Image::open_query(root)?;
    image.require_clean()?;
    let state = image.state()?;
    image.verify_catalog_archives(&state)?;
    let mut selected = selected_paths(&state, packages, paths)?;
    let targets = selected
        .iter()
        .filter_map(|&path| match &state.paths[path].node {
            Node::Hardlink { target } => Some(target.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    selected.extend(targets);
    check_state(&image.root, &state, false, |path| selected.contains(path))
}

pub(crate) fn selected_paths<'a>(
    state: &'a State,
    packages: &[String],
    paths: &[String],
) -> Result<BTreeSet<&'a str>> {
    let mut selected = BTreeSet::new();
    for path in paths {
        let path = path.strip_prefix('/').unwrap_or(path);
        validate_path(path)?;
        let (path, _) = state
            .paths
            .get_key_value(path)
            .with_context(|| format!("path is not managed: {path}"))?;
        selected.insert(path.as_str());
    }
    for package in packages {
        let selector = PackageSpec::parse(package)?;
        let installed = state
            .installed
            .get(selector.name)
            .with_context(|| format!("package is not installed: {package}"))?;
        let manifest = &installed.published.manifest;
        ensure!(
            selector
                .publisher
                .is_none_or(|publisher| publisher == manifest.publisher)
                && selector
                    .version
                    .is_none_or(|version| version == manifest.version),
            "package is not installed: {package}"
        );
        selected.extend(state.paths.iter().filter_map(|(path, managed)| {
            managed
                .owners
                .contains(selector.name)
                .then_some(path.as_str())
        }));
    }
    Ok(selected)
}

pub fn verify_pristine(path: &Path) -> Result<Vec<Finding>> {
    let image = Image::open_query(path)?;
    image.require_clean()?;
    image
        .root
        .offline_path()
        .context("pristine verification requires an inactive image")?;
    check_image(&image, true)
}

pub(crate) fn verify_image(image: &Image) -> Result<Vec<Finding>> {
    image.require_clean()?;
    check_image(image, false)
}

fn check_image(image: &Image, pristine: bool) -> Result<Vec<Finding>> {
    let state = image.state()?;
    image.verify_catalog_archives(&state)?;
    check_state(&image.root, &state, pristine, |_| true)
}

pub(crate) fn manifest_paths(state: &State) -> Result<BTreeMap<String, Managed>> {
    for (name, package) in &state.installed {
        package.verify(name, &state.publishers)?;
    }
    effective_paths(&state.settings, &state.installed)
}

pub(crate) fn verify_state(
    root: &Root,
    state: &State,
    selected: Option<&BTreeSet<&str>>,
) -> Result<Vec<Finding>> {
    check_state(root, state, false, |path| {
        selected.is_none_or(|paths| paths.contains(path))
    })
}

pub(crate) fn verify_unchanged(
    root: &Root,
    state: &State,
    changed: &BTreeSet<&str>,
) -> Result<Vec<Finding>> {
    check_state(root, state, false, |path| !changed.contains(path))
}

fn check_state(
    root: &Root,
    state: &State,
    pristine: bool,
    include: impl Fn(&str) -> bool,
) -> Result<Vec<Finding>> {
    crate::plan::freeze::check(&state.settings.freezes, &state.installed)?;
    let expected = manifest_paths(state)?;
    let mut findings = Vec::new();
    if let Err(error) = check_dependencies(&state.installed) {
        findings.push(Finding {
            is_error: true,
            message: error.to_string(),
        });
    }
    if expected != state.paths {
        findings.push(Finding {
            is_error: true,
            message: "path ownership index differs from installed manifests".into(),
        });
    }
    for (path, managed) in &expected {
        if !include(path) {
            continue;
        }
        validate_path(path)?;
        if let Node::Hardlink { target } = &managed.node {
            let message = match root.hardlink_matches(path, target) {
                Ok(true) => continue,
                Ok(false) => format!("{path}: expected a hard link to {target}"),
                Err(error) => format!("{path}: {error:#}"),
            };
            findings.push(Finding {
                is_error: true,
                message,
            });
            continue;
        }
        let observed = match root.inspect(path) {
            Ok(node) => node,
            Err(error) => {
                findings.push(Finding {
                    is_error: true,
                    message: format!("{path}: {error:#}"),
                });
                continue;
            }
        };
        if observed.as_ref() == Some(&managed.node)
            || (!pristine && managed.implicit && observed.as_ref().is_some_and(Node::is_dir))
        {
            continue;
        }
        let reason = match (&managed.node, &observed) {
            (_, None) if !pristine && managed.preserve => {
                findings.push(Finding {
                    is_error: false,
                    message: format!("locally removed config {path}"),
                });
                continue;
            }
            (
                Node::File {
                    mode,
                    uid,
                    gid,
                    capabilities,
                    ..
                },
                Some(Node::File {
                    mode: actual_mode,
                    uid: actual_uid,
                    gid: actual_gid,
                    capabilities: actual_capabilities,
                    ..
                }),
            ) if !pristine
                && managed.preserve
                && (mode, uid, gid, capabilities)
                    == (actual_mode, actual_uid, actual_gid, actual_capabilities) =>
            {
                findings.push(Finding {
                    is_error: false,
                    message: format!("locally modified config {path}"),
                });
                continue;
            }
            (_, None) => "missing",
            (file @ Node::File { .. }, Some(observed @ Node::File { .. }))
                if file.capabilities() != observed.capabilities() =>
            {
                "file capabilities differ"
            }
            _ => "content, type, or metadata differs",
        };
        findings.push(Finding {
            is_error: true,
            message: format!("{path}: {reason}"),
        });
    }
    if pristine {
        let aliases = hardlink_groups(&expected);
        root.walk(Some(".cps"), |path, entry, info| {
            let Some(managed) = expected.get(path) else {
                findings.push(Finding {
                    is_error: true,
                    message: format!("unmanaged path {path:?}"),
                });
                return Ok(false);
            };
            let kind = FileType::from_raw_mode(info.st_mode);
            let file = match &managed.node {
                Node::Hardlink { target } => expected.get(target).unwrap_or(managed),
                _ => managed,
            };
            let links = 1 + aliases.get(path).map_or(0, Vec::len);
            if kind == FileType::RegularFile
                && !matches!(managed.node, Node::Hardlink { .. })
                && info.st_nlink as usize != links
            {
                findings.push(Finding {
                    is_error: true,
                    message: format!(
                        "unexpected hard link count at {path:?}: expected {links}, found {}",
                        info.st_nlink
                    ),
                });
            }
            if matches!(kind, FileType::RegularFile | FileType::Directory)
                && Root::has_xattrs(entry, file.node.capabilities().is_some())
                    .with_context(|| format!("listing attributes at {path:?}"))?
            {
                findings.push(Finding {
                    is_error: true,
                    message: format!("unexpected extended attributes at {path:?}"),
                });
            }
            Ok(true)
        })?;
    }
    Ok(findings)
}
