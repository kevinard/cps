pub(crate) mod cgroup;
mod sandbox;
pub(crate) mod sources;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek},
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use rustix::{
    fs::{AtFlags, CWD, RenameFlags, Timespec, Timestamps, renameat_with, utimensat},
    process::geteuid,
};
use tempfile::TempDir;

use crate::{
    attestation::{self, Toolchain},
    image::root::Root,
    kdl_format::{
        build::{BuildRecipe, read},
        parse_manifest,
    },
    model::{FileDigest, Node, PackageSpec, digest, digest_reader},
    plan::{resolve_packages, select_paths},
    repo::{MANIFEST_LIMIT, read_key, read_limited, read_signing_key, snapshot, stage_blob},
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
    let BuildOptions {
        repo,
        publisher,
        trust_key,
        sources,
        output,
        check_reproducible,
        builder_signing_key,
    } = options;
    ensure!(
        builder_signing_key.is_none() || check_reproducible,
        "builder signing requires --check-reproducible"
    );
    ensure!(
        !geteuid().is_root(),
        "package builds require an unprivileged user"
    );
    ensure!(
        matches!(std::env::consts::ARCH, "x86_64" | "aarch64"),
        "the build sandbox supports x86_64 and aarch64"
    );
    let (recipe_bytes, recipe) = read(recipe_path)?;
    ensure!(
        recipe.arch == std::env::consts::ARCH,
        "toolchain architecture {} does not match this build worker",
        recipe.arch
    );
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
    let paths = manifest.payload_paths();

    let destination = fs::canonicalize(parent(output))?;
    let name = output.file_name().context("missing build output name")?;
    match destination.join(name).symlink_metadata() {
        Ok(_) => bail!("build output already exists"),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    let work = TempDir::new_in(&destination)?;
    let inputs = work.path().join("sources");
    fs::create_dir(&inputs)?;
    stage_sources(
        sources,
        &inputs,
        &recipe,
        matches!(layout, SourceLayout::Cache),
    )?;
    let script = work.path().join("build.sh");
    fs::write(&script, &recipe.script)?;
    set_mode(&script, 0o444)?;
    normalize(&script, recipe.epoch)?;
    let toolchain = work.path().join("toolchain");
    fs::create_dir(&toolchain)?;
    let resolved_toolchain = prepare_toolchain(&toolchain, repo, publisher, trust_key, &recipe)?;

    let stage = TempDir::new_in(&destination)?;
    let proto = stage.path().join("proto");
    fs::create_dir(&proto)?;
    let artifacts = run(work.path(), recipe.epoch, worker, &paths, Some(&proto))?;
    if check_reproducible {
        let repeated = run(work.path(), recipe.epoch, worker, &paths, None)?;
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

fn prepare_toolchain(
    path: &Path,
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    recipe: &BuildRecipe,
) -> Result<Toolchain> {
    let key = read_key(trust_key)?;
    let release = snapshot(repo, publisher, &key, Some(&recipe.catalog))
        .context("loading pinned toolchain catalog")?;
    let requested = recipe
        .packages
        .iter()
        .map(|name| {
            (
                name.as_str(),
                PackageSpec {
                    name,
                    publisher: None,
                    version: None,
                },
            )
        })
        .collect();
    let (installed, paths) = resolve_packages(
        &BTreeMap::new(),
        &release.packages,
        &requested,
        false,
        true,
        recipe.solver,
        |installed| {
            select_paths(
                &recipe.arch,
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
        if MOUNTS.contains(&top) {
            ensure!(
                name == top && managed.node.is_dir(),
                "toolchain path {name} conflicts with a sandbox mount"
            );
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
                let mut destination = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&file)?;
                stage_blob(repo, &managed.node, &mut destination)
                    .with_context(|| format!("materializing toolchain file {name}"))?;
                let mode = if mode & 0o111 == 0 { 0o444 } else { 0o555 };
                destination.set_permissions(fs::Permissions::from_mode(mode))?;
            }
            Node::Special => bail!("unexpected special file in toolchain"),
        }
    }
    for (name, managed) in &paths {
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
    Ok(Toolchain {
        publisher: publisher.to_owned(),
        key_sha256: digest(&key),
        catalog_sha256: release.sha256,
        package_manifests: installed
            .into_iter()
            .map(|(name, package)| (name, package.published.sha256))
            .collect(),
    })
}

fn run(
    work: &Path,
    epoch: u32,
    worker: &BuildWorker,
    paths: &BTreeSet<&str>,
    proto: Option<&Path>,
) -> Result<BTreeMap<String, FileDigest>> {
    let output = TempDir::new_in(work)?;
    sandbox::run(
        &work.join("toolchain"),
        &work.join("sources"),
        &work.join("build.sh"),
        output.path(),
        epoch,
        worker,
    )?;
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
