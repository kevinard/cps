use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use rustix::{fs::FileType, io::Errno};

use crate::{
    image::{
        FileRecords, Image, Installed, Managed, State,
        root::{Observation, Root},
    },
    model::{Node, PackageSpec, validate_path},
    plan::{check_dependencies, effective_paths, hardlink_groups},
    repo::profiles::runtime::{self, DEFAULT_LIBRARY_DIRS},
};

#[derive(Debug)]
pub struct Finding {
    pub is_error: bool,
    pub message: String,
}

/// How a check compares regular files with their manifests.
#[derive(Clone, Copy)]
pub(crate) enum Contents<'a> {
    /// Hash every regular file.
    Hash,
    /// Trust a recorded fingerprint that still matches, and hash the rest.
    Fingerprints(&'a FileRecords),
}

/// The outcome of a check, along with what it learned about file contents.
pub(crate) struct Checked {
    pub(crate) findings: Vec<Finding>,
    /// New settled fingerprints of files hashed during the check.
    pub(crate) observed: BTreeMap<String, Observation>,
    /// Recorded fingerprints that no longer match their file.
    pub(crate) stale: BTreeSet<String>,
}

impl Checked {
    pub(crate) fn errors(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|finding| finding.is_error)
    }
}

/// Check an image, hashing every managed regular file.
pub fn verify(path: &Path) -> Result<Vec<Finding>> {
    verify_image(&Image::open_query(path)?)
}

/// Check an image, trusting recorded file fingerprints that still match.
///
/// Directories, symlinks, hard links, ownership, modes, and the path index
/// are checked as in full verification. The stored package manifests are
/// trusted rather than parsed again from their signed documents. A regular file whose
/// inode, size, timestamps, mode, and owner still match the fingerprint CPS
/// recorded when it last hashed the file is not read again. Use full
/// verification to detect changes made below the filesystem, such as an
/// edited block device, or by someone who resets the system clock.
pub fn verify_quick(path: &Path) -> Result<Vec<Finding>> {
    verify_image_quick(&Image::open_query(path)?)
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

pub fn verify_selected(
    root: &Path,
    packages: &[String],
    paths: &[String],
    quick: bool,
) -> Result<Vec<Finding>> {
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
    let records = quick
        .then(|| image.file_records(Some(&selected)))
        .transpose()?;
    let contents = records
        .as_ref()
        .map_or(Contents::Hash, Contents::Fingerprints);
    let committed = quick.then_some(&state.installed);
    Ok(
        check_state(&image.root, &state, false, contents, committed, |path| {
            selected.contains(path)
        })?
        .findings,
    )
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
    check_image(&image, true, false)
}

pub(crate) fn verify_image(image: &Image) -> Result<Vec<Finding>> {
    image.require_clean()?;
    check_image(image, false, false)
}

pub(crate) fn verify_image_quick(image: &Image) -> Result<Vec<Finding>> {
    image.require_clean()?;
    check_image(image, false, true)
}

/// Fail on the first error of a quick verification of an opened image.
pub(crate) fn require_quick(image: &Image) -> Result<()> {
    for finding in verify_image_quick(image)? {
        ensure!(
            !finding.is_error,
            "image verification failed: {}",
            finding.message
        );
    }
    Ok(())
}

fn check_image(image: &Image, pristine: bool, quick: bool) -> Result<Vec<Finding>> {
    let state = image.state()?;
    image.verify_catalog_archives(&state)?;
    let records = quick.then(|| image.file_records(None)).transpose()?;
    let contents = records
        .as_ref()
        .map_or(Contents::Hash, Contents::Fingerprints);
    // Quick verification trusts committed manifest records, which CPS
    // verified before committing them, instead of parsing every manifest.
    let committed = quick.then_some(&state.installed);
    Ok(check_state(&image.root, &state, pristine, contents, committed, |_| true)?.findings)
}

/// Compute the ownership index from the installed manifests, re-verifying
/// every stored manifest against its signed KDL document and digest.
pub(crate) fn manifest_paths(state: &State) -> Result<BTreeMap<String, Managed>> {
    manifest_paths_since(state, None)
}

/// Compute the ownership index, re-verifying only manifests that differ from
/// the committed installed set.
///
/// CPS verifies every manifest before it first commits it, and transactions
/// reuse committed manifest records unchanged, so only manifests new to this
/// transaction need their KDL parsed again. Full verification still checks
/// every stored manifest.
pub(crate) fn manifest_paths_since(
    state: &State,
    committed: Option<&BTreeMap<String, Installed>>,
) -> Result<BTreeMap<String, Managed>> {
    for (name, package) in &state.installed {
        if committed
            .and_then(|committed| committed.get(name))
            .is_some_and(|old| old.published == package.published)
        {
            continue;
        }
        package.verify(name, &state.publishers)?;
    }
    let paths = effective_paths(&state.settings, &state.installed)?;
    state.settings.check_state_paths(&paths)?;
    Ok(paths)
}

/// Check managed paths of a transaction's state. `committed` is the image's
/// committed installed set, whose manifests were verified when committed.
pub(crate) fn check_transaction_state(
    root: &Root,
    state: &State,
    contents: Contents<'_>,
    committed: &BTreeMap<String, Installed>,
    include: impl Fn(&str) -> bool,
) -> Result<Checked> {
    check_state(root, state, false, contents, Some(committed), include)
}

fn check_state(
    root: &Root,
    state: &State,
    pristine: bool,
    contents: Contents<'_>,
    committed: Option<&BTreeMap<String, Installed>>,
    include: impl Fn(&str) -> bool,
) -> Result<Checked> {
    crate::plan::freeze::check(&state.settings.freezes, &state.installed)?;
    let expected = manifest_paths_since(state, committed)?;
    let mut findings = Vec::new();
    let mut fresh = BTreeMap::new();
    let mut stale = BTreeSet::new();
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
        let observed = match contents {
            Contents::Fingerprints(records) => {
                let record = records.get(path).map(|record| &record.observation);
                root.inspect_with(path, record).map(|inspected| {
                    if let Some(observation) = inspected.observed {
                        fresh.insert(path.clone(), observation);
                    } else if record.is_some() && !inspected.trusted {
                        stale.insert(path.clone());
                    }
                    inspected.node
                })
            }
            Contents::Hash => root.inspect(path),
        };
        let observed = match observed {
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
        // Triggers own what their output patterns cover, and their contents
        // are not part of any manifest.
        let generated = crate::triggers::active_outputs(&state.installed, &expected);
        // The /etc state that the systemd actions of installed packages write.
        let actions = crate::systemd::ActionOutputs::of(root, state)?;
        let var = crate::image::VAR_DIRECTORY;
        match root.dir(var) {
            Ok(directory) => {
                for entry in fs::read_dir(Root::fd_path(&directory))? {
                    let name = entry?.file_name();
                    findings.push(Finding {
                        is_error: true,
                        message: format!(
                            "unexpected path \"{var}/{}\": an inactive root leaves /var empty \
                             for tmpfiles.d to populate at boot",
                            name.to_string_lossy()
                        ),
                    });
                }
            }
            // An installer may already have mounted the /var subvolume
            // there. It is outside the image, so its contents are skipped.
            Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::XDEV) => {}
            Err(error)
                if matches!(
                    error.downcast_ref::<Errno>(),
                    Some(&(Errno::NOENT | Errno::NOTDIR | Errno::LOOP))
                ) =>
            {
                findings.push(Finding {
                    is_error: true,
                    message: format!("missing directory {var:?}, the mount point of /var"),
                });
            }
            Err(error) => return Err(error).context("inspecting the /var mount point"),
        }
        root.walk_skipping(
            &[crate::image::STATE_DIRECTORY, var],
            |path, entry, info| {
                let kind = FileType::from_raw_mode(info.st_mode);
                let Some(managed) = expected.get(path) else {
                    if kind == FileType::Directory && crate::image::STATE_ANCESTORS.contains(&path)
                    {
                        return Ok(true);
                    }
                    if crate::triggers::generated(&generated, path) {
                        return Ok(false);
                    }
                    // Descend into what the actions create, since its other
                    // entries need an owner of their own.
                    if actions.covers(path, entry, kind) {
                        return Ok(true);
                    }
                    findings.push(Finding {
                        is_error: true,
                        message: format!("unmanaged path {path:?}"),
                    });
                    return Ok(false);
                };
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
            },
        )?;
    }
    Ok(Checked {
        findings,
        observed: fresh,
        stale,
    })
}
