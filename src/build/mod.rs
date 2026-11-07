pub(crate) mod cgroup;
mod sandbox;
mod snapshot;
pub(crate) mod sources;
pub use snapshot::{GitSnapshotRef, prepare_git_snapshot, prepare_git_snapshot_ref};

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::ExitStatus,
};

use anyhow::{Context, Result, bail, ensure};
use rustix::{
    fs::{AtFlags, CWD, RenameFlags, Timespec, Timestamps, renameat_with, utimensat},
    process::geteuid,
};
use tempfile::TempDir;

use crate::{
    attestation::{self, BuildTrust, LegacyToolchain, PinnedPackage, PinnedToolchain, Toolchain},
    image::{Installed, Managed, root::Root},
    kdl_format::{
        PublisherConfig,
        build::{
            BuildRecipe, PublisherPin, ToolchainPins, read, read_toolchain_config,
            read_with_options,
        },
        parse_manifest,
    },
    model::{FileDigest, Node, digest, digest_reader},
    plan::{CandidateSet, Candidates, resolve_packages, select_paths},
    repo::{
        MANIFEST_LIMIT, read_key, read_limited, read_signing_key,
        release::fetch_repository_payloads, snapshot, stage_blob,
    },
};

const MOUNTS: &[&str] = &[
    "build", "dev", "out", "proc", "recipe", "run", "sources", "tmp",
];

pub struct BuildOptions<'a> {
    pub repo: &'a Path,
    pub publisher: &'a str,
    pub trust_key: &'a Path,
    pub sources: &'a Path,
    pub output: &'a Path,
    pub check_reproducible: bool,
    pub builder_signing_key: Option<&'a Path>,
}

pub struct BuildSettings<'a> {
    pub sources: &'a Path,
    pub output: &'a Path,
    pub check_reproducible: bool,
    pub builder_signing_key: Option<&'a Path>,
}

pub enum BuildToolchain<'a> {
    Single {
        repo: &'a Path,
        publisher: &'a str,
        trust_key: &'a Path,
    },
    Config(&'a Path),
}

/// Host-side limits for each sandbox run and the collected package outputs.
pub struct BuildWorker {
    pub(crate) timeout_secs: u64,
    pub(crate) scratch_bytes: u64,
    pub(crate) file_bytes: u64,
    pub(crate) output_bytes: u64,
    pub(crate) log_bytes: u64,
    pub(crate) cgroup: Option<cgroup::CgroupLimits>,
}

impl Default for BuildWorker {
    fn default() -> Self {
        Self {
            timeout_secs: 3600,
            scratch_bytes: 8 << 30,
            file_bytes: 8 << 30,
            output_bytes: 16 << 30,
            log_bytes: 16 << 20,
            cgroup: None,
        }
    }
}

#[derive(Clone, Copy)]
pub enum SourceLayout {
    Directory,
    Cache,
}

/// Build one package using sources and a package toolchain pinned by the recipe.
pub fn build(
    recipe_path: &Path,
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    sources: &Path,
    output: &Path,
    check_reproducible: bool,
) -> Result<String> {
    build_package(
        recipe_path,
        BuildOptions {
            repo,
            publisher,
            trust_key,
            sources,
            output,
            check_reproducible,
            builder_signing_key: None,
        },
    )
}

pub fn build_package(recipe_path: &Path, options: BuildOptions<'_>) -> Result<String> {
    build_with_worker(
        recipe_path,
        options,
        SourceLayout::Directory,
        &BuildWorker::default(),
    )
}

/// Build offline using pinned objects from a source cache.
pub fn build_cached_package(recipe_path: &Path, options: BuildOptions<'_>) -> Result<String> {
    build_with_worker(
        recipe_path,
        options,
        SourceLayout::Cache,
        &BuildWorker::default(),
    )
}

pub fn build_with_worker(
    recipe_path: &Path,
    options: BuildOptions<'_>,
    layout: SourceLayout,
    worker: &BuildWorker,
) -> Result<String> {
    build_with_recipe_options(
        recipe_path,
        BuildToolchain::Single {
            repo: options.repo,
            publisher: options.publisher,
            trust_key: options.trust_key,
        },
        BuildSettings {
            sources: options.sources,
            output: options.output,
            check_reproducible: options.check_reproducible,
            builder_signing_key: options.builder_signing_key,
        },
        layout,
        worker,
        &BTreeMap::new(),
    )
}

/// Build offline using explicitly pinned publishers and worker-local repository locations.
pub fn build_with_toolchain_config(
    recipe_path: &Path,
    config: &Path,
    settings: BuildSettings<'_>,
    layout: SourceLayout,
    worker: &BuildWorker,
) -> Result<String> {
    build_with_recipe_options(
        recipe_path,
        BuildToolchain::Config(config),
        settings,
        layout,
        worker,
        &BTreeMap::new(),
    )
}

/// Override named recipe options and record their effective values in the bundle.
pub fn build_with_recipe_options(
    recipe_path: &Path,
    location: BuildToolchain<'_>,
    settings: BuildSettings<'_>,
    layout: SourceLayout,
    worker: &BuildWorker,
    overrides: &BTreeMap<String, String>,
) -> Result<String> {
    let BuildSettings {
        sources,
        output,
        check_reproducible,
        builder_signing_key,
    } = settings;
    ensure!(
        builder_signing_key.is_none() || check_reproducible,
        "builder signing requires --check-reproducible"
    );
    let (recipe_bytes, recipe, manifest_bytes, manifest) = checked_recipe(recipe_path, overrides)?;
    let paths = manifest.payload_paths();

    let destination = fs::canonicalize(parent(output))?;
    let name = output.file_name().context("missing build output name")?;
    match destination.join(name).symlink_metadata() {
        Ok(_) => bail!("build output already exists"),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    let work = TempDir::new_in(&destination)?;
    let resolved_toolchain = prepare_workspace(work.path(), sources, location, &recipe, layout)?;

    let stage = TempDir::new_in(&destination)?;
    let proto = stage.path().join("proto");
    fs::create_dir(&proto)?;
    let artifacts = run(work.path(), &recipe, worker, &paths, Some(&proto))?;
    if check_reproducible {
        let repeated = run(work.path(), &recipe, worker, &paths, None)?;
        for (path, hash) in &artifacts {
            ensure!(
                repeated.get(path) == Some(hash),
                "build is not reproducible for {path}"
            );
        }
    }
    for (name, bytes) in [
        ("manifest.kdl", &manifest_bytes),
        ("build.kdl", &recipe_bytes),
    ] {
        let path = stage.path().join(name);
        fs::write(&path, bytes)?;
        set_mode(&path, 0o644)?;
        normalize(&path, recipe.epoch)?;
    }
    if let Some(key) = builder_signing_key {
        let bytes = attestation::sign(
            &read_signing_key(key)?,
            &manifest,
            &manifest_bytes,
            &recipe,
            &recipe_bytes,
            resolved_toolchain,
            &artifacts,
        )?;
        let path = stage.path().join("attestation.json");
        fs::write(&path, bytes)?;
        set_mode(&path, 0o644)?;
        normalize(&path, recipe.epoch)?;
    }
    normalize(stage.path(), recipe.epoch)?;
    // Flush the complete bundle before moving it into place on the same filesystem.
    Root::open(stage.path())?.sync()?;
    let parent = File::open(destination)?;
    renameat_with(
        &parent,
        stage.path().file_name().context("missing staging name")?,
        &parent,
        name,
        RenameFlags::NOREPLACE,
    )?;
    parent.sync_all()?;
    Ok(manifest.id())
}

/// Run a shell or command with a recipe's verified build inputs.
pub fn develop(
    recipe_path: &Path,
    location: BuildToolchain<'_>,
    sources: (&Path, SourceLayout),
    workspace: Option<&Path>,
    command: &[OsString],
    worker: &BuildWorker,
    overrides: &BTreeMap<String, String>,
) -> Result<ExitStatus> {
    let (_, recipe, _, _) = checked_recipe(recipe_path, overrides)?;
    let workspace = workspace.map(fs::canonicalize).transpose()?;
    let work = TempDir::new()?;
    if let Some(workspace) = &workspace {
        ensure!(
            workspace.is_dir(),
            "development workspace must be a directory"
        );
        ensure!(
            !fs::canonicalize(work.path())?.starts_with(workspace),
            "choose a workspace that does not contain the sandbox staging directory"
        );
    }
    prepare_workspace(work.path(), sources.0, location, &recipe, sources.1)?;
    let output = TempDir::new_in(work.path())?;
    sandbox::run(
        work.path(),
        output.path(),
        recipe.epoch,
        &recipe.options,
        worker,
        sandbox::Mode::Develop {
            workspace: workspace.as_deref(),
            command,
        },
    )
}

fn checked_recipe(
    recipe_path: &Path,
    overrides: &BTreeMap<String, String>,
) -> Result<(Vec<u8>, BuildRecipe, Vec<u8>, crate::model::Manifest)> {
    ensure!(
        !geteuid().is_root(),
        "package builds require an unprivileged user"
    );
    ensure!(
        matches!(std::env::consts::ARCH, "x86_64" | "aarch64"),
        "the build sandbox supports x86_64 and aarch64"
    );
    let (recipe_bytes, recipe) = read_with_options(recipe_path, overrides)?;
    ensure!(
        recipe.arch == std::env::consts::ARCH,
        "toolchain architecture {} does not match this build worker",
        recipe.arch
    );
    let (manifest_bytes, manifest) = read_recipe_manifest(recipe_path, &recipe)?;
    Ok((recipe_bytes, recipe, manifest_bytes, manifest))
}

fn read_recipe_manifest(
    recipe_path: &Path,
    recipe: &BuildRecipe,
) -> Result<(Vec<u8>, crate::model::Manifest)> {
    let manifest_bytes = read_limited(
        Root::open(parent(recipe_path))?.file(&recipe.manifest)?,
        MANIFEST_LIMIT,
    )?;
    ensure!(
        digest(&manifest_bytes) == recipe.manifest_digest,
        "manifest digest mismatch for {}",
        recipe.manifest
    );
    let manifest = parse_manifest(std::str::from_utf8(&manifest_bytes)?)?;
    manifest.validate(false)?;
    manifest.check_architecture(&recipe.arch)?;
    Ok((manifest_bytes, manifest))
}

fn prepare_workspace(
    work: &Path,
    sources: &Path,
    location: BuildToolchain<'_>,
    recipe: &BuildRecipe,
    layout: SourceLayout,
) -> Result<Toolchain> {
    let inputs = work.join("sources");
    fs::create_dir(&inputs)?;
    stage_sources(
        sources,
        &inputs,
        recipe,
        matches!(layout, SourceLayout::Cache),
    )?;
    let script = work.join("build.sh");
    fs::write(&script, &recipe.script)?;
    set_mode(&script, 0o444)?;
    normalize(&script, recipe.epoch)?;
    let toolchain = work.join("toolchain");
    fs::create_dir(&toolchain)?;
    prepare_toolchain(&toolchain, location, recipe)
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn normalize(path: &Path, epoch: u32) -> Result<()> {
    let time = Timespec {
        tv_sec: i64::from(epoch),
        tv_nsec: 0,
    };
    utimensat(
        CWD,
        path,
        &Timestamps {
            last_access: time,
            last_modification: time,
        },
        AtFlags::SYMLINK_NOFOLLOW,
    )?;
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    Ok(fs::set_permissions(path, fs::Permissions::from_mode(mode))?)
}

fn stage_sources(root: &Path, input: &Path, recipe: &BuildRecipe, cached: bool) -> Result<()> {
    let root = Root::open(root)?;
    let root = if cached {
        root.subdir(sources::OBJECTS)?
    } else {
        root
    };
    let mut directories = BTreeSet::from([input.to_owned()]);
    for (path, source) in &recipe.sources {
        let pin = &source.pin;
        let file = root
            .file(if cached { &pin.sha256 } else { path })
            .with_context(|| format!("opening build source {path}"))?;
        let staged = input.join(path);
        fs::create_dir_all(parent(&staged))?;
        directories.extend(
            staged
                .ancestors()
                .skip(1)
                .take_while(|dir| dir.starts_with(input))
                .map(Path::to_owned),
        );
        let mut destination = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&staged)?;
        sources::copy_verified(file, &mut destination, pin, path)?;
        set_mode(&staged, 0o444)?;
        normalize(&staged, recipe.epoch)?;
    }
    for directory in directories.iter().rev() {
        set_mode(directory, 0o755)?;
        normalize(directory, recipe.epoch)?;
    }
    Ok(())
}

struct ToolchainRepository {
    pin: PublisherPin,
    repository: PathBuf,
    key: Vec<u8>,
    build_trust: Option<BuildTrust>,
}

impl ToolchainRepository {
    fn configured(entry: PublisherConfig, catalog: &str, key_pin: Option<&str>) -> Result<Self> {
        let key = read_key(&entry.trust_key)?;
        let key_sha256 = digest(&key);
        ensure!(
            key_pin.is_none_or(|hash| hash == key_sha256),
            "toolchain key digest mismatch for {}",
            entry.name
        );
        let build_trust = entry
            .build_trust
            .as_ref()
            .map(|config| config.load(&key))
            .transpose()
            .with_context(|| format!("loading build trust for {}", entry.name))?;
        Ok(Self {
            pin: PublisherPin {
                name: entry.name,
                key_sha256,
                catalog_sha256: catalog.to_owned(),
            },
            repository: entry.repository,
            key,
            build_trust,
        })
    }
}

fn toolchain_repositories(
    pins: &ToolchainPins,
    location: BuildToolchain<'_>,
) -> Result<Vec<ToolchainRepository>> {
    match (pins, location) {
        (
            ToolchainPins::Single(catalog),
            BuildToolchain::Single {
                repo,
                publisher,
                trust_key,
            },
        ) => {
            let key = read_key(trust_key)?;
            Ok(vec![ToolchainRepository {
                pin: PublisherPin {
                    name: publisher.to_owned(),
                    catalog_sha256: catalog.clone(),
                    key_sha256: digest(&key),
                },
                repository: repo.to_owned(),
                key,
                build_trust: None,
            }])
        }
        (ToolchainPins::Publishers(pins), BuildToolchain::Config(config)) => {
            let mut config = read_toolchain_config(config)
                .with_context(|| format!("loading toolchain configuration {}", config.display()))?;
            ensure!(
                pins.len() == config.len(),
                "toolchain configuration does not match the pinned publishers"
            );
            pins.iter()
                .map(|pin| {
                    let entry = config.remove(&pin.name).with_context(|| {
                        format!("missing toolchain configuration for {}", pin.name)
                    })?;
                    ToolchainRepository::configured(
                        entry,
                        &pin.catalog_sha256,
                        Some(&pin.key_sha256),
                    )
                })
                .collect()
        }
        (ToolchainPins::Publishers(_), _) => {
            bail!("pinned toolchain publishers require a toolchain configuration")
        }
        (ToolchainPins::Single(catalog), BuildToolchain::Config(config)) => {
            let mut entries = read_toolchain_config(config)?;
            ensure!(
                entries.len() == 1,
                "a legacy toolchain catalog requires one configured publisher"
            );
            let (_, entry) = entries
                .pop_first()
                .context("missing toolchain repository")?;
            Ok(vec![ToolchainRepository::configured(entry, catalog, None)?])
        }
    }
}

struct ResolvedToolchain {
    repositories: Vec<ToolchainRepository>,
    installed: BTreeMap<String, Installed>,
    paths: BTreeMap<String, Managed>,
}

fn resolve_toolchain(
    location: BuildToolchain<'_>,
    recipe: &BuildRecipe,
) -> Result<ResolvedToolchain> {
    let repositories = toolchain_repositories(&recipe.toolchain, location)?;
    let requested = recipe.requested()?;
    let mut sources: BTreeMap<_, _> = requested
        .values()
        .filter_map(|spec| {
            spec.publisher
                .map(|publisher| (spec.name.to_owned(), publisher))
        })
        .collect();
    let mut candidates = CandidateSet::default();
    let mut builds = BTreeMap::new();
    for source in &repositories {
        let pin = &source.pin;
        let release = snapshot(
            &source.repository,
            &pin.name,
            &source.key,
            Some(&pin.catalog_sha256),
        )
        .with_context(|| format!("loading pinned toolchain catalog for {}", pin.name))?;
        candidates.add_obsoletions(&mut sources, &pin.name, &release.obsoletions);
        candidates.add_packages(&mut sources, &pin.name, &recipe.arch, release.packages);
        if let Some(trust) = &source.build_trust {
            builds.insert(
                pin.name.as_str(),
                (&source.repository, trust, release.builds),
            );
        }
    }
    let (installed, paths) = resolve_packages(
        &BTreeMap::new(),
        Candidates {
            packages: &candidates.packages,
            obsoletions: &candidates.obsoletions,
            freezes: &BTreeMap::new(),
            profile_packages: &BTreeMap::new(),
        },
        &requested,
        false,
        crate::plan::Retention::None,
        recipe.solver,
        |installed| {
            select_paths(
                &recipe.arch,
                &BTreeMap::new(),
                &BTreeMap::new(),
                Node::Dir {
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                },
                installed,
            )
        },
    )
    .context("resolving build toolchain")?;
    for (name, managed) in &paths {
        let (top, _) = name.split_once('/').unwrap_or((name, ""));
        ensure!(
            !MOUNTS.contains(&top) || (name == top && managed.node.is_dir()),
            "toolchain path {name} conflicts with a sandbox mount"
        );
    }
    for package in installed.values() {
        let published = &package.published;
        if let Some((repository, trust, evidence)) =
            builds.get(published.manifest.publisher.as_str())
        {
            evidence.verify(repository, [*trust], published)?;
        }
    }
    Ok(ResolvedToolchain {
        repositories,
        installed,
        paths,
    })
}

pub(crate) fn verify_toolchain(
    config: &Path,
    recipe: &BuildRecipe,
    claimed: &Toolchain,
) -> Result<()> {
    let selection = resolve_toolchain(BuildToolchain::Config(config), recipe)?;
    ensure!(
        &selection.evidence(&recipe.toolchain)? == claimed,
        "attested toolchain differs from the independently resolved toolchain"
    );
    Ok(())
}

/// Verify the pinned build selection and fetch only its required toolchain payloads.
pub fn fetch_toolchain(recipe_path: &Path, config: &Path) -> Result<usize> {
    let (_, recipe) = read(recipe_path)?;
    let selection = resolve_toolchain(BuildToolchain::Config(config), &recipe)?;
    let mut required = BTreeMap::<_, BTreeMap<_, u64>>::new();
    for managed in selection.paths.values() {
        if let Node::File { sha256, size, .. } = &managed.node {
            let publisher = selection.file_publisher(managed)?;
            let length = required
                .entry(publisher)
                .or_default()
                .entry(sha256.as_str())
                .or_insert(*size);
            ensure!(
                *length == *size,
                "conflicting toolchain payload lengths for {sha256}"
            );
        }
    }
    let mut count = 0;
    for source in &selection.repositories {
        if let Some(blobs) = required.remove(source.pin.name.as_str()) {
            count += blobs.len();
            fetch_repository_payloads(&source.repository, &source.pin.name, &source.key, blobs)
                .with_context(|| format!("fetching pinned toolchain for {}", source.pin.name))?;
        }
    }
    ensure!(
        required.is_empty(),
        "toolchain payload has no configured publisher"
    );
    Ok(count)
}

fn prepare_toolchain(
    path: &Path,
    location: BuildToolchain<'_>,
    recipe: &BuildRecipe,
) -> Result<Toolchain> {
    let selection = resolve_toolchain(location, recipe)?;
    let paths_by_publisher: BTreeMap<_, _> = selection
        .repositories
        .iter()
        .map(|source| (source.pin.name.as_str(), source.repository.as_path()))
        .collect();
    let paths = &selection.paths;
    for (name, managed) in paths {
        let (top, _) = name.split_once('/').unwrap_or((name, ""));
        if MOUNTS.contains(&top) {
            continue;
        }
        let file = path.join(name);
        match &managed.node {
            Node::Dir { .. } => {
                fs::create_dir(&file)?;
                set_mode(&file, 0o755)?;
            }
            Node::Symlink { target, .. } => symlink(target, &file)?,
            Node::Hardlink { .. } => {}
            Node::File { mode, .. } => {
                let publisher = selection.file_publisher(managed)?;
                let mut destination = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&file)?;
                stage_blob(
                    paths_by_publisher[publisher],
                    &managed.node,
                    &mut destination,
                )
                .with_context(|| format!("materializing toolchain file {name}"))?;
                let mode = if mode & 0o111 == 0 { 0o444 } else { 0o555 };
                destination.set_permissions(fs::Permissions::from_mode(mode))?;
            }
            Node::Special => bail!("unexpected special file in toolchain"),
        }
    }
    for (name, managed) in paths {
        if let Node::Hardlink { target } = &managed.node {
            fs::hard_link(path.join(target), path.join(name))
                .with_context(|| format!("linking toolchain file {name} to {target}"))?;
        }
    }
    for mount in MOUNTS {
        fs::create_dir(path.join(mount))?;
        set_mode(&path.join(mount), 0o755)?;
        normalize(&path.join(mount), recipe.epoch)?;
    }
    // The mount destination must exist before the root is mounted read-only.
    File::create(path.join("recipe/build.sh"))?;
    normalize(&path.join("recipe"), recipe.epoch)?;
    for name in paths.keys().rev() {
        normalize(&path.join(name), recipe.epoch)?;
    }
    set_mode(path, 0o755)?;
    normalize(path, recipe.epoch)?;
    selection.evidence(&recipe.toolchain)
}

impl ResolvedToolchain {
    fn file_publisher(&self, managed: &Managed) -> Result<&str> {
        let owner = managed.owners.first().context("unowned toolchain file")?;
        Ok(&self.installed[owner].published.manifest.publisher)
    }

    fn evidence(self, pins: &ToolchainPins) -> Result<Toolchain> {
        let Self {
            repositories,
            installed,
            ..
        } = self;
        match pins {
            ToolchainPins::Single(_) => {
                let pin = repositories
                    .into_iter()
                    .next()
                    .context("missing toolchain repository")?
                    .pin;
                Ok(Toolchain::Legacy(LegacyToolchain {
                    publisher: pin.name,
                    key_sha256: pin.key_sha256,
                    catalog_sha256: pin.catalog_sha256,
                    package_manifests: installed
                        .into_iter()
                        .map(|(name, package)| (name, package.published.sha256))
                        .collect(),
                }))
            }
            ToolchainPins::Publishers(_) => Ok(Toolchain::Pinned(PinnedToolchain {
                publishers: repositories.into_iter().map(|source| source.pin).collect(),
                package_manifests: installed
                    .into_iter()
                    .map(|(name, package)| {
                        let published = package.published;
                        (
                            name,
                            PinnedPackage {
                                publisher: published.manifest.publisher,
                                version: published.manifest.version,
                                sha256: published.sha256,
                            },
                        )
                    })
                    .collect(),
            })),
        }
    }
}

fn run(
    work: &Path,
    recipe: &BuildRecipe,
    worker: &BuildWorker,
    paths: &BTreeSet<&str>,
    proto: Option<&Path>,
) -> Result<BTreeMap<String, FileDigest>> {
    let output = TempDir::new_in(work)?;
    let status = sandbox::run(
        work,
        output.path(),
        recipe.epoch,
        &recipe.options,
        worker,
        sandbox::Mode::Build,
    )?;
    ensure!(status.success(), "isolated build failed with {status}");
    let epoch = recipe.epoch;
    let root = Root::open(output.path())?;
    let mut artifacts = BTreeMap::new();
    let mut directories: BTreeSet<_> = proto.into_iter().map(Path::to_owned).collect();
    let mut remaining = worker.output_bytes;
    for path in paths {
        let source = root
            .file(path)
            .with_context(|| format!("opening declared build output {path}"))?;
        let size = source.metadata()?.len();
        remaining = remaining.checked_sub(size).with_context(|| {
            format!(
                "declared outputs exceed {} bytes at {path}",
                worker.output_bytes
            )
        })?;
        let mut source = source.take(size.saturating_add(1));
        let artifact = if let Some(proto) = proto {
            let destination = proto.join(path);
            fs::create_dir_all(parent(&destination))?;
            directories.extend(
                destination
                    .ancestors()
                    .skip(1)
                    .take_while(|dir| dir.starts_with(proto))
                    .map(Path::to_owned),
            );
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&destination)?;
            ensure!(
                io::copy(&mut source, &mut file)? == size,
                "build output changed size at {path}"
            );
            file.rewind()?;
            let hash = digest_reader(&mut file)?;
            set_mode(&destination, 0o644)?;
            normalize(&destination, epoch)?;
            FileDigest { sha256: hash, size }
        } else {
            FileDigest {
                sha256: digest_reader(&mut source)?,
                size,
            }
        };
        artifacts.insert((*path).to_owned(), artifact);
    }
    for directory in directories.iter().rev() {
        set_mode(directory, 0o755)?;
        normalize(directory, epoch)?;
    }
    Ok(artifacts)
}
