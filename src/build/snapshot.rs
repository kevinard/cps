use std::{
    ffi::OsStr,
    fs::{self, File},
    io,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{RenameFlags, renameat_with};
use tempfile::TempDir;

use crate::{
    image::root::Root,
    kdl_format::{build, manifest_text},
    model::{FileDigest, Version, digest, digest_reader},
    repo::{MANIFEST_LIMIT, assign_git_version, lock_snapshot_series, read_key},
};

use super::{parent, read_recipe_manifest};

pub(crate) const ARCHIVE: &str = "git.tar";

#[derive(Clone, Copy, Debug)]
pub enum GitSnapshotRef<'a> {
    Branch(&'a str),
    Tag(&'a str),
    Commit(&'a str),
}

impl GitSnapshotRef<'_> {
    fn tag_version(self) -> Option<Version> {
        let Self::Tag(tag) = self else {
            return None;
        };
        tag.strip_prefix('v').unwrap_or(tag).parse().ok()
    }

    /// Describe the source recorded in the snapshot manifest.
    pub fn provenance(self, commit: &str) -> String {
        match self {
            Self::Branch(branch) => format!("{branch}@{commit}"),
            Self::Tag(tag) => format!("tag:{tag}@{commit}"),
            Self::Commit(_) => commit.to_owned(),
        }
    }
}

/// Prepare a pinned build recipe for the selected branch tip and reserve its package version.
pub fn prepare_git_snapshot(
    recipe_path: &Path,
    git_repo: &OsStr,
    branch: &str,
    repo: &Path,
    trust_key: &Path,
    output: &Path,
) -> Result<(String, String)> {
    prepare_git_snapshot_ref(
        recipe_path,
        git_repo,
        GitSnapshotRef::Branch(branch),
        repo,
        trust_key,
        output,
    )
}

/// Prepare a pinned build recipe for a branch, tag, or full commit ID.
pub fn prepare_git_snapshot_ref(
    recipe_path: &Path,
    git_repo: &OsStr,
    source: GitSnapshotRef<'_>,
    repo: &Path,
    trust_key: &Path,
    output: &Path,
) -> Result<(String, String)> {
    let destination = fs::canonicalize(parent(output))?;
    let name = output.file_name().context("missing snapshot output name")?;
    match destination.join(name).symlink_metadata() {
        Ok(_) => bail!("snapshot output already exists"),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    let key = read_key(trust_key)?;
    let (template, recipe) = build::read(recipe_path)?;
    let (_, mut manifest) = read_recipe_manifest(recipe_path, &recipe)?;
    let stage = TempDir::new_in(&destination)?;
    let sources = stage.path().join("sources");
    fs::create_dir(&sources)?;
    // Serialize ref resolution and version allocation for this package.
    let _series = lock_snapshot_series(repo, &manifest)?;
    let (commit, archive) = archive_revision(git_repo, source, &sources.join(ARCHIVE))?;

    let upstream = source.provenance(&commit);
    manifest.upstream_version = Some(upstream.clone());
    manifest.validate(false)?;
    let tag_version = source.tag_version();
    let fingerprint = if tag_version.is_some() {
        digest(upstream.as_bytes())
    } else {
        digest(format!("{upstream}\n{}\n{}", archive.sha256, digest(&template)).as_bytes())
    };
    manifest.version = assign_git_version(repo, &key, &manifest, tag_version, &fingerprint)?;
    let manifest_text = manifest_text(&manifest);
    ensure!(
        manifest_text.len() as u64 <= MANIFEST_LIMIT,
        "snapshot manifest exceeds the size limit"
    );
    let recipe_text = build::snapshot_recipe(
        &template,
        &digest(manifest_text.as_bytes()),
        ARCHIVE,
        &archive,
    )?;
    fs::write(stage.path().join("manifest.kdl"), manifest_text)?;
    fs::write(stage.path().join("build.kdl"), recipe_text)?;

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
    Ok((manifest.id(), commit))
}

pub(crate) fn git() -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_ALLOW_PROTOCOL", "file:https:ssh")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1");
    for variable in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(variable);
    }
    command
}

fn archive_revision(
    repo: &OsStr,
    source: GitSnapshotRef<'_>,
    archive: &Path,
) -> Result<(String, FileDigest)> {
    let work = TempDir::new_in(parent(archive))?;
    let clone = work.path().join("repo");
    let commit = checkout(repo, source, &clone, false, git)?;
    let digest = archive_commit(&clone, &commit, archive, git)?;
    Ok((commit, digest))
}

pub(crate) fn checkout(
    repo: &OsStr,
    source: GitSnapshotRef<'_>,
    clone: &Path,
    full_history: bool,
    command: fn() -> Command,
) -> Result<String> {
    let reference = match source {
        GitSnapshotRef::Branch(branch) => clone_branch(repo, branch, clone, full_history, command)?,
        GitSnapshotRef::Tag(tag) => {
            let reference = checked_ref("tags", tag, command)?;
            let format = tag_object_format(repo, &reference, command)?;
            fetch_revision(repo, clone, &reference, format, command)?;
            "FETCH_HEAD".to_owned()
        }
        GitSnapshotRef::Commit(sha) => {
            let format = object_format(sha)
                .context("expected a full Git commit SHA (40 or 64 hex digits)")?;
            fetch_revision(repo, clone, &sha.to_ascii_lowercase(), format, command)?;
            "FETCH_HEAD".to_owned()
        }
    };
    let result = command()
        .arg("-C")
        .arg(clone)
        .args(["rev-parse", "--verify", &format!("{reference}^{{commit}}")])
        .stderr(Stdio::inherit())
        .output()
        .context("running git rev-parse")?;
    ensure!(
        result.status.success(),
        "resolving {source:?} to a Git commit failed"
    );
    let commit = String::from_utf8(result.stdout)?.trim_end().to_owned();
    ensure!(
        object_format(&commit).is_some(),
        "git returned an invalid commit ID"
    );
    if let GitSnapshotRef::Commit(requested) = source {
        ensure!(
            requested.eq_ignore_ascii_case(&commit),
            "Git object {requested} does not identify a commit directly"
        );
    }

    Ok(commit)
}

pub(crate) fn archive_commit(
    clone: &Path,
    commit: &str,
    archive: &Path,
    command: fn() -> Command,
) -> Result<FileDigest> {
    let status = command()
        .arg("-C")
        .arg(clone)
        .args([
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "tar.umask=0002",
            "archive",
            "--format=tar",
            commit,
        ])
        .stdout(File::create(archive)?)
        .status()
        .context("running git archive")?;
    ensure!(status.success(), "archiving Git commit {commit} failed");
    let file = File::open(archive)?;
    let size = file.metadata()?.len();
    let sha256 = digest_reader(file)?;
    Ok(FileDigest { size, sha256 })
}

fn checked_ref(namespace: &str, name: &str, command: fn() -> Command) -> Result<String> {
    let reference = format!("refs/{namespace}/{name}");
    ensure!(
        !name.starts_with('-')
            && command()
                .args(["check-ref-format", &reference])
                .status()
                .context("running git check-ref-format")?
                .success(),
        "invalid Git reference {reference:?}"
    );
    Ok(reference)
}

fn clone_branch(
    repo: &OsStr,
    branch: &str,
    clone: &Path,
    full_history: bool,
    command: fn() -> Command,
) -> Result<String> {
    let reference = checked_ref("heads", branch, command)?;
    let mut git = command();
    if !full_history {
        git.args(["clone", "--depth=1"]);
    } else {
        git.arg("clone");
    }
    let status = git
        .args([
            "--quiet",
            "--bare",
            "--single-branch",
            "--no-tags",
            "--no-local",
            "--template=",
            "--branch",
            branch,
            "--",
        ])
        .arg(repo)
        .arg(clone)
        .status()
        .context("running git clone")?;
    ensure!(status.success(), "cloning Git branch {branch:?} failed");
    Ok(reference)
}

pub(crate) fn object_format(oid: &str) -> Option<&'static str> {
    if !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    match oid.len() {
        40 => Some("sha1"),
        64 => Some("sha256"),
        _ => None,
    }
}

fn tag_object_format(
    repo: &OsStr,
    reference: &str,
    command: fn() -> Command,
) -> Result<&'static str> {
    // Initialize the temporary repository with the tag's object format.
    let result = command()
        .args(["ls-remote", "--refs", "--exit-code", "--"])
        .arg(repo)
        .arg(reference)
        .stderr(Stdio::inherit())
        .output()
        .context("running git ls-remote")?;
    ensure!(
        result.status.success(),
        "looking up Git tag {reference:?} failed"
    );
    String::from_utf8(result.stdout)?
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find_map(|(oid, name)| (name == reference).then_some(oid))
        .and_then(object_format)
        .with_context(|| format!("Git did not return a valid object ID for {reference:?}"))
}

fn fetch_revision(
    repo: &OsStr,
    clone: &Path,
    reference: &str,
    format: &str,
    command: fn() -> Command,
) -> Result<()> {
    let status = command()
        .args([
            "init",
            "--quiet",
            "--bare",
            "--template=",
            &format!("--object-format={format}"),
        ])
        .arg(clone)
        .status()
        .context("running git init")?;
    ensure!(
        status.success(),
        "initializing temporary Git repository failed"
    );

    let status = command()
        // Keep the caller's working directory so relative Git repository paths resolve correctly.
        .arg("--git-dir")
        .arg(clone)
        .args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-recurse-submodules",
            "--depth=1",
            "--",
        ])
        .arg(repo)
        .arg(reference)
        .status()
        .context("running git fetch")?;
    ensure!(
        status.success(),
        "fetching Git revision {reference:?} failed"
    );
    Ok(())
}
