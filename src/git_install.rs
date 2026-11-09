use std::{
    ffi::OsStr,
    fs::{self, File},
    os::unix::{
        fs::{MetadataExt, PermissionsExt, chown},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};
use rustix::process::geteuid;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use url::Url;

use crate::{
    BuildSettings, BuildWorker, GitSnapshotRef, SourceLayout,
    build::snapshot::{ARCHIVE, archive_commit, checkout, git, object_format},
    build_with_toolchain_config, fetch_sources_with_local,
    kdl_format::{
        build::{RECIPE_LIMIT, git_document, git_toolchain_template, snapshot_recipe},
        manifest_text,
    },
    model::{FileDigest, Manifest, digest},
    publish,
    repo::{read_key, read_signing_key, snapshot},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GitSource {
    url: String,
    reference: GitReference,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum GitReference {
    Commit(String),
    Branch(String),
}

impl GitSource {
    pub(crate) fn new(repo: &str, reference: GitSnapshotRef<'_>) -> Result<Self> {
        let url = if repo.contains("://") {
            Url::parse(repo).context("invalid Git repository URL")?
        } else {
            Url::from_file_path(fs::canonicalize(repo).context("opening local Git repository")?)
                .map_err(|()| anyhow::anyhow!("invalid local Git repository path"))?
        };
        ensure!(
            matches!(url.scheme(), "https" | "file")
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
                && url.query().is_none(),
            "use an HTTPS Git URL without credentials or a local Git repository"
        );
        let reference = match reference {
            GitSnapshotRef::Commit(commit) => {
                ensure!(
                    object_format(commit).is_some(),
                    "expected a full Git commit ID"
                );
                GitReference::Commit(commit.to_ascii_lowercase())
            }
            GitSnapshotRef::Branch(branch) => GitReference::Branch(branch.to_owned()),
            GitSnapshotRef::Tag(_) => bail!("Git installs require --commit or --track-branch"),
        };
        Ok(Self {
            url: url.into(),
            reference,
        })
    }

    pub(crate) fn publisher(&self) -> String {
        let (kind, value) = match &self.reference {
            GitReference::Commit(commit) => ("commit", commit),
            GitReference::Branch(branch) => ("branch", branch),
        };
        format!(
            "git-{}",
            digest(format!("{}\n{kind}\n{value}", self.url).as_bytes())
        )
    }

    pub(crate) fn branch(&self) -> Option<&str> {
        match &self.reference {
            GitReference::Branch(branch) => Some(branch),
            GitReference::Commit(_) => None,
        }
    }
}

#[derive(Debug)]
pub struct GitChange {
    pub package: String,
    pub url: String,
    pub branch: Option<String>,
    pub previous: Option<String>,
    pub commit: String,
}

pub(crate) struct GitBuild {
    work: TempDir,
    template: String,
    archive: FileDigest,
    pub manifest: Manifest,
    pub change: GitChange,
}

fn install_git() -> Command {
    let mut command = git();
    command
        .env("GIT_ALLOW_PROTOCOL", "file:https")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_EXEC_PATH")
        .env_remove("GIT_ASKPASS")
        .env_remove("GIT_SSL_NO_VERIFY");
    command
}

impl GitBuild {
    pub(crate) fn prepare(source: &GitSource, previous: Option<&Manifest>) -> Result<Option<Self>> {
        let old = previous
            .map(|manifest| {
                let commit = manifest
                    .upstream_version
                    .as_deref()
                    .context("installed Git package is missing its commit")?;
                ensure!(
                    object_format(commit).is_some(),
                    "installed Git package has an invalid commit"
                );
                Ok(commit)
            })
            .transpose()?;
        let reference = match &source.reference {
            GitReference::Commit(commit) => GitSnapshotRef::Commit(commit),
            GitReference::Branch(branch) => GitSnapshotRef::Branch(branch),
        };
        let checkout_dir = tempfile::tempdir()?;
        fs::set_permissions(checkout_dir.path(), fs::Permissions::from_mode(0o700))?;
        let clone = checkout_dir.path().join("repo");
        let commit = checkout(
            OsStr::new(&source.url),
            reference,
            &clone,
            old.is_some(),
            install_git,
        )?;
        if let Some(old) = old {
            if old == commit {
                return Ok(None);
            }
            let status = install_git()
                .arg("-C")
                .arg(&clone)
                .args(["merge-base", "--is-ancestor", old, &commit])
                .status()?;
            ensure!(
                status.success(),
                "branch does not descend from the installed Git commit {old}"
            );
        }
        let object = format!("{commit}:cps.kdl");
        let size = install_git()
            .arg("-C")
            .arg(&clone)
            .args(["cat-file", "-s", &object])
            .output()?;
        ensure!(
            size.status.success(),
            "Git commit {commit} has no cps.kdl at its root"
        );
        let size: u64 = std::str::from_utf8(&size.stdout)?.trim().parse()?;
        ensure!(size <= RECIPE_LIMIT, "cps.kdl exceeds the size limit");
        let contents = install_git()
            .arg("-C")
            .arg(&clone)
            .args(["cat-file", "blob", &object])
            .output()?;
        ensure!(
            contents.status.success(),
            "reading cps.kdl at {commit} failed"
        );
        let (mut manifest, template) =
            git_document(std::str::from_utf8(&contents.stdout)?, &source.publisher())
                .with_context(|| format!("reading cps.kdl at {commit}"))?;
        ensure!(
            previous.is_none_or(|previous| previous.name == manifest.name),
            "tracked Git source changed the installed package name"
        );
        manifest.upstream_version = Some(commit.clone());
        manifest.validate(false)?;
        let work = tempfile::tempdir()?;
        fs::set_permissions(work.path(), fs::Permissions::from_mode(0o700))?;
        let sources = work.path().join("sources");
        fs::create_dir(&sources)?;
        let archive = archive_commit(&clone, &commit, &sources.join(ARCHIVE), install_git)?;
        let change = GitChange {
            package: manifest.name.clone(),
            url: source.url.clone(),
            branch: source.branch().map(str::to_owned),
            previous: old.map(str::to_owned),
            commit,
        };
        Ok(Some(Self {
            work,
            template,
            archive,
            manifest,
            change,
        }))
    }

    pub(crate) fn publish(
        mut self,
        source: &GitSource,
        store: &GitStore,
        toolchain: &Path,
    ) -> Result<String> {
        let key = read_key(&store.public_key())?;
        if store.repo().join("current").try_exists()? {
            let catalog = snapshot(&store.repo(), &self.manifest.publisher, &key, None)?;
            let mut newest = None;
            for package in catalog.packages {
                ensure!(
                    package.manifest.name == self.manifest.name,
                    "Git source changed package name"
                );
                if package.manifest.upstream_version == self.manifest.upstream_version {
                    return Ok(package.manifest.id());
                }
                newest = newest.max(Some(package.manifest.version));
            }
            ensure!(
                source.branch().is_some(),
                "pinned Git source already contains another commit"
            );
            if let Some(newest) = newest
                && self.manifest.version.semantic() <= newest.semantic()
            {
                self.manifest.version = newest.next_source_revision()?;
            }
        }
        let manifest = manifest_text(&self.manifest);
        let template = git_toolchain_template(&self.template, toolchain)?;
        let recipe = snapshot_recipe(
            template.as_bytes(),
            &digest(manifest.as_bytes()),
            ARCHIVE,
            &self.archive,
        )?;
        let path = self.work.path();
        fs::write(path.join("manifest.kdl"), manifest)?;
        fs::write(path.join("build.kdl"), recipe)?;
        build_package(path, toolchain)?;
        publish(
            &store.repo(),
            &path.join("worker/bundle/proto"),
            &path.join("manifest.kdl"),
            &store.private_key(),
        )
    }
}

fn build_package(path: &Path, toolchain: &Path) -> Result<()> {
    let worker = path.join("worker");
    fs::create_dir(&worker)?;
    let recipe = path.join("build.kdl");
    let sources = path.join("sources");
    let cache = worker.join("sources");
    let output = worker.join("bundle");
    if !geteuid().is_root() {
        fetch_sources_with_local(&recipe, &cache, &sources)?;
        build_with_toolchain_config(
            &recipe,
            toolchain,
            BuildSettings {
                sources: &cache,
                output: &output,
                check_reproducible: false,
                builder_signing_key: None,
            },
            SourceLayout::Cache,
            &BuildWorker::default(),
        )?;
        return Ok(());
    }
    let uid = std::env::var("SUDO_UID")
        .context("run Git package builds with sudo from an unprivileged account")?
        .parse()?;
    let gid = std::env::var("SUDO_GID")
        .context("missing sudo user's group for the Git build")?
        .parse()?;
    ensure!(
        uid != 0 && gid != 0,
        "Git builds require an unprivileged build user"
    );
    for directory in [path, sources.as_path()] {
        chown(directory, None, Some(gid))?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o750))?;
    }
    for input in [&recipe, &path.join("manifest.kdl"), &sources.join(ARCHIVE)] {
        chown(input, None, Some(gid))?;
        fs::set_permissions(input, fs::Permissions::from_mode(0o640))?;
    }
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700))?;
    chown(&worker, Some(uid), Some(gid))?;
    let command = || -> Result<Command> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .uid(uid)
            .gid(gid)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &worker)
            .env("TMPDIR", &worker)
            .current_dir(&worker)
            .stdin(Stdio::null());
        Ok(command)
    };
    let status = command()?
        .args(["source", "fetch", "--from"])
        .arg(&sources)
        .arg("--cache")
        .arg(&cache)
        .arg(&recipe)
        .status()?;
    ensure!(
        status.success(),
        "Git package source preparation failed with {status}"
    );
    let status = command()?
        .args(["build", "--toolchain-config"])
        .arg(toolchain)
        .arg("--source-cache")
        .arg(&cache)
        .arg("--output")
        .arg(&output)
        .arg(&recipe)
        .status()?;
    ensure!(status.success(), "Git package build failed with {status}");
    Ok(())
}

pub(crate) struct GitStore(PathBuf);

impl GitStore {
    pub(crate) fn open(path: PathBuf) -> Result<Self> {
        let parent = path
            .parent()
            .context("missing Git publisher cache parent")?;
        fs::create_dir_all(parent)?;
        let info = parent.symlink_metadata()?;
        ensure!(
            info.is_dir() && info.uid() == geteuid().as_raw() && info.mode() & 0o022 == 0,
            "Git cache parent must be owned and writable only by the CPS account"
        );
        if !path.try_exists()? {
            let stage = TempDir::new_in(parent)?;
            fs::set_permissions(stage.path(), fs::Permissions::from_mode(0o700))?;
            let key = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
                .map_err(|_| anyhow::anyhow!("generating Git publisher signing key"))?;
            let private = stage.path().join("private.der");
            fs::write(&private, key.as_ref())?;
            fs::set_permissions(&private, fs::Permissions::from_mode(0o600))?;
            let key = read_signing_key(&private)?;
            let mut public = hex::decode("302a300506032b6570032100")?;
            public.extend_from_slice(key.public_key().as_ref());
            fs::write(stage.path().join("public.der"), public)?;
            fs::create_dir(stage.path().join("repo"))?;
            File::open(private)?.sync_all()?;
            File::open(stage.path().join("public.der"))?.sync_all()?;
            File::open(stage.path())?.sync_all()?;
            fs::rename(stage.path(), &path)?;
            File::open(parent)?.sync_all()?;
        }
        let info = path.symlink_metadata()?;
        ensure!(
            info.is_dir() && info.uid() == geteuid().as_raw() && info.mode() & 0o077 == 0,
            "Git publisher cache must be a private directory owned by the CPS account"
        );
        Ok(Self(path))
    }

    pub(crate) fn repo(&self) -> PathBuf {
        self.0.join("repo")
    }
    pub(crate) fn public_key(&self) -> PathBuf {
        self.0.join("public.der")
    }
    fn private_key(&self) -> PathBuf {
        self.0.join("private.der")
    }
}
