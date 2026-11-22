use std::{
    ffi::OsStr,
    fs::{self, File},
    os::unix::{
        fs::{MetadataExt, PermissionsExt, chown},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
};

use anyhow::{Context, Result, bail, ensure};

use rustix::process::geteuid;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use url::Url;

use crate::{
    BuildSettings, BuildToolchain, BuildWorker, GitSnapshotRef, SourceLayout,
    build::{
        read_expanded_output,
        snapshot::{ARCHIVE, archive_commit, checkout, git, object_format},
    },
    fetch_sources_with_local,
    image::{accounts::Accounts, root::Root},
    kdl_format::{
        build::{
            RECIPE_LIMIT, git_document, git_toolchain_template, parse as parse_recipe,
            parse_template, snapshot_recipe,
        },
        manifest_text,
    },
    model::{FileDigest, Manifest, digest},
    repo::{publish_from_root, read_signing_key, read_trust_root, snapshot},
};

/// Renew the local Git publisher's trust root this long before it expires.
const RENEW_BEFORE_DAYS: i64 = 90;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GitSource {
    url: String,
    reference: GitReference,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signatures: Option<GitSignatures>,
}

/// Require a signed commit, or a signed tag on the commit, from an allowed signer.
///
/// The policy names a file that the administrator maintains: an SSH
/// allowed-signers file or an OpenPGP public keyring. It applies when the
/// package is installed and on every tracked-branch upgrade.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct GitSignatures {
    format: GitSignatureFormat,
    allowed_signers: PathBuf,
    require: GitSignedObject,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum GitSignatureFormat {
    Ssh,
    Openpgp,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum GitSignedObject {
    Commit,
    Tag,
}

impl GitSignatures {
    /// Parse `ssh:PATH` or `openpgp:PATH`.
    pub(crate) fn new(signers: &str, require: GitSignedObject) -> Result<Self> {
        let (format, path) = signers
            .split_once(':')
            .context("use ssh:PATH or openpgp:PATH for the allowed signers")?;
        let format = match format {
            "ssh" => GitSignatureFormat::Ssh,
            "openpgp" => GitSignatureFormat::Openpgp,
            _ => bail!("allowed signers format must be ssh or openpgp"),
        };
        let path = fs::canonicalize(path)
            .with_context(|| format!("opening allowed signers file {path}"))?;
        let policy = Self {
            format,
            allowed_signers: path,
            require,
        };
        policy.check_file()?;
        Ok(policy)
    }

    /// Only the CPS account may change who can sign.
    fn check_file(&self) -> Result<()> {
        let path = &self.allowed_signers;
        let info = path
            .metadata()
            .with_context(|| format!("opening allowed signers file {}", path.display()))?;
        ensure!(
            info.is_file() && info.uid() == geteuid().as_raw() && info.mode() & 0o022 == 0,
            "allowed signers file {} must be a regular file owned and writable only by the CPS account",
            path.display()
        );
        Ok(())
    }

    pub(crate) fn describe(&self) -> String {
        let format = match self.format {
            GitSignatureFormat::Ssh => "SSH",
            GitSignatureFormat::Openpgp => "OpenPGP",
        };
        let object = match self.require {
            GitSignedObject::Commit => "commit",
            GitSignedObject::Tag => "tag",
        };
        format!(
            "{format} {object} signature verified against {}",
            self.allowed_signers.display()
        )
    }

    /// Check the signature that the policy requires on `commit`.
    ///
    /// The build account fetches tags and reports raw Git objects, but it
    /// cannot vouch for them: CPS checks that each object's bytes hash to
    /// its ID, separates the signature from the signed payload as Git does,
    /// and verifies it itself with `ssh-keygen` or `gpg` against the signers
    /// file, in a directory that only the CPS account can write.
    fn verify(&self, url: &OsStr, clone: &Path, commit: &str) -> Result<()> {
        self.check_file()?;
        let scratch = tempfile::tempdir()?;
        fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o700))?;
        let keyring = scratch.path().join("keyring");
        if self.format == GitSignatureFormat::Openpgp {
            // A private keyring holds only the allowed keys.
            fs::create_dir(&keyring)?;
            fs::set_permissions(&keyring, fs::Permissions::from_mode(0o700))?;
            let status = signature_tool("gpg")
                .env("GNUPGHOME", &keyring)
                .args(["--batch", "--quiet", "--import"])
                .arg(&self.allowed_signers)
                .status()
                .context("running gpg to load the allowed signers")?;
            ensure!(
                status.success(),
                "importing the allowed OpenPGP signers failed"
            );
        }
        let result = (|| match self.require {
            GitSignedObject::Commit => {
                let object = read_object(clone, "commit", commit)?;
                let signed = match commit_signature(&object, commit) {
                    Some((payload, signature)) => {
                        self.signed_by_allowed(scratch.path(), &payload, &signature)?
                    }
                    None => false,
                };
                ensure!(
                    signed,
                    "Git commit {commit} has no valid signature from an allowed signer"
                );
                Ok(())
            }
            GitSignedObject::Tag => {
                let status = install_git()
                    .arg("-C")
                    .arg(clone)
                    .args(["fetch", "--quiet", "--no-write-fetch-head", "--"])
                    .arg(url)
                    .arg("+refs/tags/*:refs/tags/*")
                    .status()
                    .context("fetching Git tags")?;
                ensure!(status.success(), "fetching Git tags failed");
                let tags = install_git()
                    .arg("-C")
                    .arg(clone)
                    .args([
                        "for-each-ref",
                        "--format=%(objecttype) %(objectname)",
                        "--points-at",
                        commit,
                        "refs/tags",
                    ])
                    .output()
                    .context("listing Git tags")?;
                ensure!(tags.status.success(), "listing Git tags failed");
                for line in std::str::from_utf8(&tags.stdout)?.lines() {
                    // Lightweight tags cannot carry a signature.
                    let Some(object) = line.strip_prefix("tag ") else {
                        continue;
                    };
                    ensure!(
                        object_format(object).is_some(),
                        "git returned an invalid tag ID"
                    );
                    let tag = read_object(clone, "tag", object)?;
                    if !tag.starts_with(format!("object {commit}\ntype commit\n").as_bytes()) {
                        continue;
                    }
                    if let Some((payload, signature)) = tag_signature(&tag)
                        && self.signed_by_allowed(scratch.path(), payload, signature)?
                    {
                        return Ok(());
                    }
                }
                bail!("Git commit {commit} has no tag signed by an allowed signer")
            }
        })();
        if self.format == GitSignatureFormat::Openpgp {
            let _ = signature_tool("gpgconf")
                .env("GNUPGHOME", &keyring)
                .args(["--kill", "all"])
                .status();
        }
        result
    }

    /// Whether `signature` over `payload` comes from an allowed signer.
    fn signed_by_allowed(&self, scratch: &Path, payload: &[u8], signature: &[u8]) -> Result<bool> {
        let payload_path = scratch.join("payload");
        let signature_path = scratch.join("signature");
        fs::write(&payload_path, payload)?;
        fs::write(&signature_path, signature)?;
        match self.format {
            GitSignatureFormat::Ssh => {
                if !signature.starts_with(b"-----BEGIN SSH SIGNATURE-----") {
                    return Ok(false);
                }
                let principals = signature_tool("ssh-keygen")
                    .args(["-Y", "find-principals", "-f"])
                    .arg(&self.allowed_signers)
                    .arg("-s")
                    .arg(&signature_path)
                    .output()
                    .context("running ssh-keygen to find the signer")?;
                if !principals.status.success() {
                    return Ok(false);
                }
                for principal in std::str::from_utf8(&principals.stdout)?.lines() {
                    let status = signature_tool("ssh-keygen")
                        .args(["-Y", "verify", "-n", "git", "-f"])
                        .arg(&self.allowed_signers)
                        .arg("-I")
                        .arg(principal)
                        .arg("-s")
                        .arg(&signature_path)
                        .stdin(File::open(&payload_path)?)
                        .stdout(Stdio::null())
                        .status()
                        .context("running ssh-keygen to verify the signature")?;
                    if status.success() {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            GitSignatureFormat::Openpgp => {
                if !signature.starts_with(b"-----BEGIN PGP SIGNATURE-----") {
                    return Ok(false);
                }
                let output = signature_tool("gpg")
                    .env("GNUPGHOME", scratch.join("keyring"))
                    .args(["--batch", "--no-tty", "--status-fd", "1", "--verify"])
                    .arg(&signature_path)
                    .arg(&payload_path)
                    .output()
                    .context("running gpg to verify the signature")?;
                let status = String::from_utf8_lossy(&output.stdout);
                let has = |keyword: &str| {
                    status
                        .lines()
                        .any(|line| line.starts_with(&format!("[GNUPG:] {keyword} ")))
                };
                Ok(output.status.success()
                    && has("GOODSIG")
                    && has("VALIDSIG")
                    && !has("BADSIG")
                    && !has("ERRSIG"))
            }
        }
    }
}

/// A signature tool that CPS runs itself, with a clean environment.
fn signature_tool(program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    command
}

/// Commit and tag objects larger than this are refused.
const OBJECT_LIMIT: u64 = 1 << 20;

/// Read a Git object that the build account reports, and check that its
/// bytes are the object `id` names.
fn read_object(clone: &Path, kind: &str, id: &str) -> Result<Vec<u8>> {
    let format = object_format(id).context("invalid Git object ID")?;
    let output = install_git()
        .arg("-C")
        .arg(clone)
        .args(["cat-file", kind, id])
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("reading Git {kind} {id}"))?;
    ensure!(
        output.status.success() && output.stdout.len() as u64 <= OBJECT_LIMIT,
        "reading Git {kind} {id} failed"
    );
    let object = output.stdout;
    let mut framed = format!("{kind} {}\0", object.len()).into_bytes();
    framed.extend_from_slice(&object);
    let algorithm = if format == "sha256" {
        &aws_lc_rs::digest::SHA256
    } else {
        &aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY
    };
    ensure!(
        hex::encode(aws_lc_rs::digest::digest(algorithm, &framed)) == id.to_ascii_lowercase(),
        "Git {kind} {id} does not match its ID"
    );
    Ok(object)
}

/// Split a commit into the payload its signature covers and the signature,
/// as Git does: every signature header is left out of the payload, and the
/// one for the commit's object format holds the signature.
fn commit_signature(object: &[u8], id: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let header = if id.len() == 64 {
        "gpgsig-sha256"
    } else {
        "gpgsig"
    };
    let mut payload = Vec::new();
    let mut signature = Vec::new();
    let mut in_headers = true;
    let mut current: Option<bool> = None;
    for line in object.split_inclusive(|byte| *byte == b'\n') {
        if !in_headers {
            payload.extend_from_slice(line);
            continue;
        }
        if line == b"\n" {
            in_headers = false;
            current = None;
            payload.extend_from_slice(line);
            continue;
        }
        if let Some(rest) = line.strip_prefix(b" ")
            && let Some(ours) = current
        {
            if ours {
                signature.extend_from_slice(rest);
            }
            continue;
        }
        current = None;
        for name in ["gpgsig", "gpgsig-sha256"] {
            if let Some(rest) = line.strip_prefix(format!("{name} ").as_bytes()) {
                let ours = name == header;
                if ours {
                    signature.extend_from_slice(rest);
                }
                current = Some(ours);
            }
        }
        if current.is_none() {
            payload.extend_from_slice(line);
        }
    }
    (!signature.is_empty()).then_some((payload, signature))
}

/// Split a tag into its payload and the signature at the end of its
/// message, which starts at the last line that opens a signature block.
fn tag_signature(object: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut start = None;
    let mut offset = 0;
    for line in object.split_inclusive(|byte| *byte == b'\n') {
        if line.starts_with(b"-----BEGIN PGP SIGNATURE-----")
            || line.starts_with(b"-----BEGIN SSH SIGNATURE-----")
        {
            start = Some(offset);
        }
        offset += line.len();
    }
    start.map(|start| object.split_at(start))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum GitReference {
    Commit(String),
    Branch(String),
}

impl GitSource {
    pub(crate) fn new(
        repo: &str,
        reference: GitSnapshotRef<'_>,
        signatures: Option<GitSignatures>,
    ) -> Result<Self> {
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
            signatures,
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
pub(crate) struct GitChange {
    pub package: String,
    pub url: String,
    pub branch: Option<String>,
    pub previous: Option<String>,
    pub commit: String,
    /// The signature policy this commit satisfied, if any.
    pub signature: Option<String>,
}

pub(crate) struct GitBuild {
    work: TempDir,
    template: String,
    archive: FileDigest,
    pub manifest: Manifest,
    pub change: GitChange,
}

/// The account that fetches and builds Git packages when CPS runs as root.
/// The cps package creates it with sysusers.d.
const BUILD_ACCOUNT: &str = "cps-build";

static ACCOUNT: OnceLock<Option<(u32, u32)>> = OnceLock::new();

/// The user and group IDs of the build account when CPS runs as root, and
/// `None` otherwise, when Git and the build run as the calling user.
///
/// Root never runs Git on a source: a repository is untrusted input, and
/// Git runs code that a repository's own configuration names. Root only
/// reads what the build account reports, hashes the source archive, and
/// checks the build output against the template CPS wrote.
fn build_account() -> Result<Option<(u32, u32)>> {
    if let Some(account) = ACCOUNT.get() {
        return Ok(*account);
    }
    let account = if geteuid().is_root() {
        let ids = Accounts::new(File::open("/")?)
            .ids(BUILD_ACCOUNT, BUILD_ACCOUNT)
            .context(
                "CPS running as root fetches and builds Git packages as the cps-build account, \
                 which the cps package's sysusers.d creates",
            )?;
        ensure!(
            ids.0 != 0 && ids.1 != 0,
            "the {BUILD_ACCOUNT} account must not be root"
        );
        Some(ids)
    } else {
        None
    };
    Ok(*ACCOUNT.get_or_init(|| account))
}

/// Run a command as the build account when CPS runs as root.
fn as_builder(command: &mut Command) -> &mut Command {
    if let Some(Some((uid, gid))) = ACCOUNT.get() {
        command
            .uid(*uid)
            .gid(*gid)
            .current_dir("/")
            .env("HOME", "/");
    }
    command
}

/// Hand a private directory to the build account when CPS runs as root.
fn give_to_builder(path: &Path) -> Result<()> {
    if let Some((uid, gid)) = build_account()? {
        chown(path, Some(uid), Some(gid))?;
    }
    Ok(())
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
    as_builder(&mut command);
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
        build_account()?;
        let checkout_dir = tempfile::tempdir()?;
        fs::set_permissions(checkout_dir.path(), fs::Permissions::from_mode(0o700))?;
        give_to_builder(checkout_dir.path())?;
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
        // Without a signature policy, anyone who can push to the branch is trusted.
        if let Some(signatures) = &source.signatures {
            signatures.verify(OsStr::new(&source.url), &clone, &commit)?;
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
            signature: source.signatures.as_ref().map(GitSignatures::describe),
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
        let key = store.ensure_trust(&self.manifest.publisher)?;
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
        fs::write(path.join("manifest.kdl"), &manifest)?;
        fs::write(path.join("build.kdl"), &recipe)?;
        build_package(path, toolchain)?;
        let (proto, expanded) = checked_output(path, &manifest, &recipe)?;
        let mut ids =
            publish_from_root(&store.repo(), &proto, vec![expanded], &store.private_key())?;
        Ok(ids.remove(0))
    }
}

/// Open the build output without following symlinks and check its expanded
/// manifest against the template that CPS wrote.
///
/// When CPS runs as root, the cps-build account owns `worker/`, so the
/// output is untrusted until the expanded manifest is shown to be what the
/// template produces. The expanded manifest also lists the split debug
/// information.
fn checked_output(work: &Path, template: &str, recipe: &str) -> Result<(Root, Manifest)> {
    let recipe = parse_recipe(recipe)?;
    let template = parse_template(template, &recipe)?;
    let bundle = Root::open(work)?
        .subdir("worker/bundle")
        .context("opening the Git build output")?;
    let manifest = read_expanded_output(&bundle, &recipe, &template)?;
    Ok((bundle.subdir("proto")?, manifest))
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
        crate::build::build(
            &recipe,
            BuildToolchain::Config(toolchain),
            BuildSettings {
                sources: &cache,
                output: &output,
                check_reproducible: false,
                builder_signing_key: None,
            },
            SourceLayout::Cache,
            &BuildWorker::default(),
            &std::collections::BTreeMap::new(),
        )?;
        return Ok(());
    }
    let (uid, gid) = build_account()?.context("expected the Git build account")?;
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
            let key = crate::repo::generate_signing_key()?;
            let private = stage.path().join("private.der");
            fs::write(&private, key)?;
            fs::set_permissions(&private, fs::Permissions::from_mode(0o600))?;
            read_signing_key(&private)?;
            fs::create_dir(stage.path().join("repo"))?;
            File::open(private)?.sync_all()?;
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
    /// The pinned version 1 trust root of the local Git publisher.
    pub(crate) fn trust_root(&self) -> PathBuf {
        crate::repo::trust_root_path(&self.repo(), 1)
    }

    /// Create the local 1-of-1 trust root on first use, renew it before it
    /// expires, and return the pinned trust root bytes.
    fn ensure_trust(&self, publisher: &str) -> Result<Vec<u8>> {
        if !self.trust_root().try_exists()? {
            crate::repo::init_trust(&self.repo(), publisher, &self.private_key(), None)?;
        }
        crate::repo::renew_trust(&self.repo(), &self.private_key(), RENEW_BEFORE_DAYS)?;
        read_trust_root(&self.trust_root())
    }
    fn private_key(&self) -> PathBuf {
        self.0.join("private.der")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) -> Result<String> {
        let output = install_git()
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.org"])
            .args(["-c", "gpg.format=ssh", "-c", "commit.gpgSign=false"])
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        ensure!(output.status.success(), "git {args:?} failed");
        Ok(String::from_utf8(output.stdout)?.trim_end().to_owned())
    }

    #[test]
    fn git_build_output_is_checked_against_the_template() -> Result<()> {
        use std::os::unix::fs::symlink;
        let hash = digest(b"recipe input");
        let recipe = format!(
            r#"schema-version 1 kind="build-recipe"
build architecture="x86_64" epoch=1700000000 {{
    toolchain solver="sat-v2" {{
        publisher "seed" catalog-sha256="{hash}" key-sha256="{hash}"
        package "build/compiler"
    }}
    manifest "manifest.kdl" sha256="{hash}"
    script "true"
}}
"#
        );
        let template = r#"schema-version 1 kind="package-manifest"
package "test/git" publisher="git" version="1.0.0-0" {
    file "usr/share/git" mode="0644"
}
"#;
        let work = tempfile::tempdir()?;
        let path = work.path();
        let bundle = path.join("worker/bundle");
        let manifests = bundle.join("manifests");
        fs::create_dir_all(&manifests)?;
        fs::create_dir(bundle.join("proto"))?;
        let expanded = manifests.join("test/git.kdl");
        fs::create_dir(manifests.join("test"))?;
        fs::write(&expanded, template)?;
        let (_, manifest) = checked_output(path, template, &recipe)?;
        assert_eq!(manifest.name, "test/git");

        // The build account cannot add actions that the template lacks.
        fs::write(
            &expanded,
            template.replace("}\n", "    file \"usr/bin/su\" mode=\"4755\"\n}\n"),
        )?;
        let Err(error) = checked_output(path, template, &recipe) else {
            panic!("an added setuid file was accepted");
        };
        assert!(
            format!("{error:#}").contains("no collect rule"),
            "{error:#}"
        );

        // Nor point root at another file or another tree through a symlink.
        let elsewhere = path.join("elsewhere.kdl");
        fs::write(&elsewhere, template)?;
        fs::remove_file(&expanded)?;
        symlink(&elsewhere, &expanded)?;
        assert!(checked_output(path, template, &recipe).is_err());
        fs::remove_file(&expanded)?;
        fs::write(&expanded, template)?;
        fs::rename(&bundle, path.join("real"))?;
        symlink(path.join("real"), &bundle)?;
        assert!(checked_output(path, template, &recipe).is_err());
        Ok(())
    }

    #[test]
    fn signature_policies_accept_only_allowed_signers() -> Result<()> {
        if Command::new("ssh-keygen").arg("-?").output().is_err() {
            eprintln!("skipping: ssh-keygen is unavailable");
            return Ok(());
        }
        let work = tempfile::tempdir()?;
        let dir = work.path();
        for name in ["good", "bad"] {
            let status = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(dir.join(name))
                .status()?;
            ensure!(status.success());
        }
        let allowed = dir.join("allowed_signers");
        fs::write(
            &allowed,
            format!(
                "alice@example.org namespaces=\"git\" {}",
                fs::read_to_string(dir.join("good.pub"))?
            ),
        )?;
        fs::set_permissions(&allowed, fs::Permissions::from_mode(0o644))?;
        let repo = dir.join("repo");
        fs::create_dir(&repo)?;
        run(&repo, &["init", "-q"])?;
        let key = |name: &str| {
            format!(
                "user.signingkey={}",
                dir.join(format!("{name}.pub")).display()
            )
        };
        let commit = |name: Option<&str>, message: &str| -> Result<String> {
            fs::write(repo.join("file"), message)?;
            run(&repo, &["add", "file"])?;
            match name {
                Some(name) => run(
                    &repo,
                    &["-c", &key(name), "commit", "-q", "-S", "-m", message],
                )?,
                None => run(&repo, &["commit", "-q", "-m", message])?,
            };
            run(&repo, &["rev-parse", "HEAD"])
        };
        let good = commit(Some("good"), "good")?;
        let bad = commit(Some("bad"), "bad")?;
        let unsigned = commit(None, "unsigned")?;
        run(
            &repo,
            &[
                "-c",
                &key("good"),
                "tag",
                "-s",
                "-m",
                "release",
                "v1",
                &unsigned,
            ],
        )?;
        run(
            &repo,
            &["-c", &key("bad"), "tag", "-s", "-m", "forged", "v0", &bad],
        )?;
        run(&repo, &["tag", "light", &good])?;
        let clone = dir.join("clone");
        let status = install_git()
            .args(["clone", "--quiet", "--bare", "--no-tags", "--"])
            .arg(&repo)
            .arg(&clone)
            .status()?;
        ensure!(status.success());
        let url = repo.as_os_str();

        let spec = format!("ssh:{}", allowed.display());
        let commits = GitSignatures::new(&spec, GitSignedObject::Commit)?;
        commits.verify(url, &clone, &good)?;
        for rejected in [&bad, &unsigned] {
            let error = commits.verify(url, &clone, rejected).unwrap_err();
            assert!(error.to_string().contains("no valid signature"));
        }
        let tags = GitSignatures::new(&spec, GitSignedObject::Tag)?;
        tags.verify(url, &clone, &unsigned)?;
        for rejected in [&good, &bad] {
            let error = tags.verify(url, &clone, rejected).unwrap_err();
            assert!(error.to_string().contains("no tag signed"));
        }

        assert!(
            GitSignatures::new(
                &format!("x509:{}", allowed.display()),
                GitSignedObject::Commit
            )
            .is_err()
        );
        fs::set_permissions(&allowed, fs::Permissions::from_mode(0o664))?;
        assert!(GitSignatures::new(&spec, GitSignedObject::Commit).is_err());
        let error = commits.verify(url, &clone, &good).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("writable only by the CPS account")
        );
        Ok(())
    }

    #[test]
    fn openpgp_policies_accept_only_allowed_signers() -> Result<()> {
        if Command::new("gpg").arg("--version").output().is_err() {
            eprintln!("skipping: gpg is unavailable");
            return Ok(());
        }
        let work = tempfile::tempdir()?;
        let dir = work.path();
        let home = |name: &str| -> Result<PathBuf> {
            let home = dir.join(name);
            fs::create_dir(&home)?;
            fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;
            let status = Command::new("gpg")
                .env("GNUPGHOME", &home)
                .args(["--batch", "--quiet", "--passphrase", "", "--quick-gen-key"])
                .arg(format!("{name} <{name}@example.org>"))
                .args(["ed25519", "sign", "never"])
                .stdin(Stdio::null())
                .status()?;
            ensure!(status.success(), "generating a {name} key failed");
            Ok(home)
        };
        let good = home("good")?;
        let bad = home("bad")?;
        let allowed = dir.join("allowed.asc");
        let export = Command::new("gpg")
            .env("GNUPGHOME", &good)
            .args(["--batch", "--armor", "--export"])
            .output()?;
        fs::write(&allowed, export.stdout)?;
        fs::set_permissions(&allowed, fs::Permissions::from_mode(0o644))?;
        let repo = dir.join("repo");
        fs::create_dir(&repo)?;
        run(&repo, &["init", "-q"])?;
        let commit = |home: &Path, message: &str| -> Result<String> {
            let signer = format!(
                "user.signingkey={}@example.org",
                home.file_name().unwrap().display()
            );
            fs::write(repo.join("file"), message)?;
            run(&repo, &["add", "file"])?;
            let status = install_git()
                .arg("-C")
                .arg(&repo)
                .env("GNUPGHOME", home)
                .args(["-c", "user.name=Test", "-c", "user.email=test@example.org"])
                .args(["-c", &signer, "-c", "gpg.format=openpgp"])
                .args(["commit", "-q", "-S", "-m", message])
                .stdin(Stdio::null())
                .status()?;
            ensure!(status.success(), "signing a commit failed");
            run(&repo, &["rev-parse", "HEAD"])
        };
        let signed = commit(&good, "good")?;
        let forged = commit(&bad, "bad")?;
        let policy = GitSignatures::new(
            &format!("openpgp:{}", allowed.display()),
            GitSignedObject::Commit,
        )?;
        policy.verify(repo.as_os_str(), &repo, &signed)?;
        let error = policy.verify(repo.as_os_str(), &repo, &forged).unwrap_err();
        assert!(
            error.to_string().contains("no valid signature"),
            "{error:#}"
        );
        for home in [good, bad] {
            let _ = Command::new("gpgconf")
                .env("GNUPGHOME", home)
                .args(["--kill", "all"])
                .status();
        }
        Ok(())
    }
}
