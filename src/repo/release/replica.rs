//! Keep a full replica of a CPS web root, verified before it is served.
//!
//! A replica reads the source like a client: it follows the signed
//! timestamp to the snapshot, the targets roles, and every target they list.
//! It reuses unchanged files from the served tree by hard link once their
//! digests match, or once a private record shows they are the very files it
//! verified before, unchanged since. It keeps the previous release's files for clients that are
//! mid-download, and verifies the staged tree against the TUF root before it
//! replaces the served one. A source that serves expired, forged, or older
//! metadata leaves the replica unchanged, and a source that serves what the
//! replica already has leaves it untouched.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::{Client, StatusCode};
use rustix::fs::{FlockOperation, RenameFlags, flock, renameat_with};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tough::RepositoryLoader;
use url::Url;

use super::{
    DESCRIPTOR, DESCRIPTOR_LIMIT, ROLE_LIMIT, ROOT_LIMIT, TIMESTAMP_LIMIT, TUF_LIMITS,
    mirrors::{get_limited, mirror_base},
    read_tuf_root,
    web_files::{create_public, create_public_dir, fd_url, link_or_copy, write_public},
};
use crate::{
    model::digest_reader,
    repo::{lock_file, read_limited},
    transport::https_client,
};

const DOWNLOADS: usize = 8;
const MAX_TARGETS: usize = 1 << 20;

/// Where a replica copies from: a served web root on this host or over HTTPS.
enum Source {
    Local(PathBuf),
    Remote(Client, Url),
}

impl Source {
    fn open(source: &str) -> Result<(Self, Option<File>)> {
        if source.starts_with('/') {
            let path = fs::canonicalize(source)
                .with_context(|| format!("opening the source web root {source}"))?;
            // The origin's deployment holds this lock while it publishes.
            let lock = File::open(&path)?;
            flock(&lock, FlockOperation::LockShared)?;
            return Ok((Self::Local(path), Some(lock)));
        }
        let url = mirror_base(source)?;
        ensure!(
            url.scheme() == "https",
            "replicate from an absolute path or an https:// URL"
        );
        Ok((Self::Remote(https_client().build()?, url), None))
    }

    fn describe(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::Remote(_, url) => url.to_string(),
        }
    }

    /// A small file, or `None` when the source does not have it.
    async fn read(&self, name: &str, limit: u64) -> Result<Option<Vec<u8>>> {
        let result = match self {
            Self::Local(path) => read_optional(&path.join(name), limit),
            Self::Remote(client, base) => get_limited(client, base.join(name)?, limit).await,
        };
        result.with_context(|| format!("reading {name} from {}", self.describe()))
    }

    /// Copy a signed target, checking its length and digest as it arrives.
    async fn copy(&self, name: &str, destination: &Path, length: u64, sha256: &str) -> Result<()> {
        let mut file = create_public(destination)?;
        let mut hasher = Sha256::new();
        let mut written = 0u64;
        let mut write = |chunk: &[u8]| -> Result<()> {
            written += chunk.len() as u64;
            ensure!(written <= length, "{name} exceeds its signed length");
            hasher.update(chunk);
            file.write_all(chunk)?;
            Ok(())
        };
        match self {
            Self::Local(path) => {
                let mut source = File::open(path.join(name))?;
                let mut buffer = vec![0; 1 << 16];
                loop {
                    let count = std::io::Read::read(&mut source, &mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    write(&buffer[..count])?;
                }
            }
            Self::Remote(client, base) => {
                let response = client.get(base.join(name)?).send().await?;
                ensure!(
                    response.status() == StatusCode::OK,
                    "HTTP status {} for {name}",
                    response.status()
                );
                let mut chunks = response.bytes_stream();
                while let Some(chunk) = chunks.try_next().await? {
                    write(&chunk)?;
                }
            }
        }
        ensure!(
            written == length && hex::encode(hasher.finalize()) == sha256,
            "{name} does not match its signed length and digest"
        );
        file.sync_all()?;
        Ok(())
    }
}

/// A small local file, or `None` when it does not exist.
fn read_optional(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    match File::open(path) {
        Ok(file) => read_limited(file, limit).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The files one signed release consists of.
#[derive(Default)]
struct Release {
    /// The snapshot file name, which identifies the release's files.
    snapshot: String,
    /// Versioned metadata files, by name, with their bytes.
    metadata: BTreeMap<String, Vec<u8>>,
    /// Target files, by consistent-snapshot name, with length and SHA-256.
    targets: BTreeMap<String, (u64, String)>,
}

fn version_of(document: &serde_json::Value, role: &str) -> Result<u64> {
    document["signed"]["version"]
        .as_u64()
        .filter(|version| *version >= 1)
        .with_context(|| format!("invalid {role} version"))
}

fn parse(bytes: &[u8], name: &str) -> Result<serde_json::Value> {
    serde_json::from_slice(bytes).with_context(|| format!("parsing {name}"))
}

/// Follow a timestamp to every file of its release, reading through `read`.
async fn walk<F, Fut>(timestamp: &[u8], read: F) -> Result<Release>
where
    F: Fn(String, u64) -> Fut,
    Fut: std::future::Future<Output = Result<Option<Vec<u8>>>>,
{
    let document = parse(timestamp, "timestamp.json")?;
    version_of(&document, "timestamp")?;
    let mut release = Release::default();
    let snapshot_version = document["signed"]["meta"]["snapshot.json"]["version"]
        .as_u64()
        .context("the timestamp lists no snapshot")?;
    let snapshot_name = format!("{snapshot_version}.snapshot.json");
    let snapshot = read(format!("metadata/{snapshot_name}"), ROLE_LIMIT)
        .await?
        .with_context(|| format!("missing {snapshot_name}"))?;
    let meta = parse(&snapshot, &snapshot_name)?["signed"]["meta"]
        .as_object()
        .context("invalid snapshot")?
        .clone();
    release.snapshot.clone_from(&snapshot_name);
    release.metadata.insert(snapshot_name, snapshot);
    for (role, entry) in meta {
        ensure!(
            role.ends_with(".json")
                && role
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
            "unexpected role {role} in the snapshot"
        );
        let version = entry["version"]
            .as_u64()
            .with_context(|| format!("invalid version for {role}"))?;
        let name = format!("{version}.{role}");
        let bytes = read(format!("metadata/{name}"), ROLE_LIMIT)
            .await?
            .with_context(|| format!("missing {name}"))?;
        if let Some(expected) = entry["hashes"]["sha256"].as_str() {
            ensure!(
                hex::encode(Sha256::digest(&bytes)) == expected,
                "{name} does not match the snapshot"
            );
        }
        for (target, details) in parse(&bytes, &name)?["signed"]["targets"]
            .as_object()
            .context("invalid targets role")?
        {
            let length = details["length"]
                .as_u64()
                .with_context(|| format!("invalid length for target {target}"))?;
            let sha256 = details["hashes"]["sha256"]
                .as_str()
                .filter(|hash| {
                    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .with_context(|| format!("invalid digest for target {target}"))?;
            ensure!(
                !target.is_empty()
                    && target
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
                "unsupported target name {target}"
            );
            release
                .targets
                .insert(format!("{sha256}.{target}"), (length, sha256.to_owned()));
            ensure!(release.targets.len() <= MAX_TARGETS, "too many targets");
        }
        release.metadata.insert(name, bytes);
    }
    Ok(release)
}

/// The release a served tree currently holds, if it is readable.
fn served_release(web: &Path) -> Option<Release> {
    let timestamp = fs::read(web.join("metadata/timestamp.json")).ok()?;
    let runtime = tokio::runtime::Builder::new_current_thread().build().ok()?;
    runtime
        .block_on(walk(&timestamp, |name, limit| async move {
            read_optional(&web.join(name), limit)
        }))
        .ok()
}

/// Identifies one file on disk as it was when it was verified. Writing to
/// the file changes its modification time; replacing it changes its inode.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Stamp {
    inode: u64,
    length: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl Stamp {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            inode: metadata.ino(),
            length: metadata.len(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
        }
    }
}

/// The served targets this replica verified, kept beside the web root.
#[derive(Default, Deserialize, Serialize)]
struct Verified {
    targets: BTreeMap<String, Stamp>,
}

const VERIFIED_LIMIT: u64 = 256 << 20;

impl Verified {
    /// A missing or unreadable record only means files are hashed again.
    fn read(path: &Path) -> Self {
        read_optional(path, VERIFIED_LIMIT)
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Record the served release's targets after a successful swap.
    fn record(path: &Path, web: &Path, release: &Release) -> Result<()> {
        let mut verified = Self::default();
        for file in release.targets.keys() {
            let metadata = fs::metadata(web.join("targets").join(file))?;
            verified.targets.insert(file.clone(), Stamp::of(&metadata));
        }
        let directory = path.parent().context("missing record directory")?;
        let mut temp = tempfile::NamedTempFile::new_in(directory)?;
        temp.write_all(&serde_json::to_vec(&verified)?)?;
        temp.as_file().sync_all()?;
        temp.persist(path)?;
        Ok(())
    }
}

/// Whether a served target can be reused: it is the file verified before,
/// unchanged since, or its length and digest match.
fn intact(path: &Path, name: &str, length: u64, sha256: &str, verified: &Verified) -> Result<bool> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != length {
        return Ok(false);
    }
    if verified.targets.get(name) == Some(&Stamp::of(&metadata)) {
        return Ok(true);
    }
    Ok(digest_reader(std::io::Read::take(file, length))? == sha256)
}

/// Update a replica of `source` at `output`. Returns a summary line.
pub(crate) fn replicate(source: &str, tuf_root: &Path, output: &Path) -> Result<String> {
    let anchor = read_tuf_root(tuf_root)?;
    let name = output
        .file_name()
        .context("expected a web root directory")?
        .to_owned();
    let parent = fs::canonicalize(
        output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )?;
    let web = parent.join(&name);
    ensure!(web != Path::new("/"), "use a dedicated web root");
    let _lock = lock_file(&parent.join(format!(".{}.replica.lock", name.to_string_lossy())))?;
    let record = parent.join(format!(".{}.replica-verified.json", name.to_string_lossy()));
    let (source, _source_lock) = Source::open(source)?;
    if let Source::Local(path) = &source {
        ensure!(
            !path.starts_with(&web) && !web.starts_with(path),
            "use separate source and replica directories"
        );
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let served_timestamp = read_optional(&web.join("metadata/timestamp.json"), TIMESTAMP_LIMIT)?;
    let served_version = served_timestamp.as_ref().and_then(|bytes| {
        let version =
            parse(bytes, "timestamp.json").and_then(|document| version_of(&document, "timestamp"));
        if let Err(error) = &version {
            eprintln!("Warning: the served replica's timestamp is unreadable: {error:#}");
        }
        version.ok()
    });
    let (timestamp, descriptor) = runtime.block_on(async {
        let timestamp = source
            .read("metadata/timestamp.json", TIMESTAMP_LIMIT)
            .await?
            .context("the source has no metadata/timestamp.json; is it a deployed CPS web root?")?;
        let descriptor = source.read(DESCRIPTOR, DESCRIPTOR_LIMIT).await?;
        anyhow::Ok((timestamp, descriptor))
    })?;
    let version = version_of(&parse(&timestamp, "timestamp.json")?, "timestamp")?;
    if let Some(served) = served_version {
        ensure!(
            version >= served,
            "the source serves timestamp version {version}, older than the replica's {served}"
        );
        ensure!(
            version > served || served_timestamp.as_ref() == Some(&timestamp),
            "the source reused timestamp version {version} with different contents"
        );
        // Nothing changed: leave the served tree, and the previous release it keeps, alone.
        if version == served
            && read_optional(&web.join(DESCRIPTOR), DESCRIPTOR_LIMIT)? == descriptor
        {
            return Ok(format!(
                "The replica at {} already serves TUF timestamp version {version} from {}",
                web.display(),
                source.describe()
            ));
        }
    }

    let stage = tempfile::Builder::new()
        .prefix(".cps-replica.")
        .tempdir_in(&parent)?;
    let tree = stage.path().join("tree");
    for directory in ["", "metadata", "targets"] {
        create_public_dir(&tree.join(directory))?;
    }
    let served = served_release(&web);
    let verified = Verified::read(&record);
    let (summary, release) = runtime.block_on(async {
        let release = walk(&timestamp, |name, limit| {
            let source = &source;
            async move { source.read(&name, limit).await }
        })
        .await?;
        // Clients walk the root history from the version they trust.
        let mut roots = Vec::new();
        for version in 1..=TUF_LIMITS.max_root_updates + 1 {
            let file = format!("{version}.root.json");
            match source.read(&format!("metadata/{file}"), ROOT_LIMIT).await? {
                Some(bytes) => roots.push((file, bytes)),
                None => break,
            }
        }
        ensure!(!roots.is_empty(), "the source serves no 1.root.json");

        for (file, bytes) in roots
            .iter()
            .map(|(file, bytes)| (file, bytes))
            .chain(&release.metadata)
        {
            write_public(&tree.join("metadata").join(file), bytes)?;
        }
        write_public(&tree.join("metadata/timestamp.json"), &timestamp)?;
        if let Some(descriptor) = &descriptor {
            write_public(&tree.join(DESCRIPTOR), descriptor)?;
        }
        let mut wanted = Vec::new();
        for (file, (length, sha256)) in &release.targets {
            let existing = web.join("targets").join(file);
            // A damaged served file is fetched again rather than carried forward.
            if intact(&existing, file, *length, sha256, &verified)? {
                link_or_copy(&existing, &tree.join("targets").join(file))?;
            } else {
                wanted.push((file, *length, sha256));
            }
        }
        let fetched = wanted.len();
        stream::iter(wanted)
            .map(|(file, length, sha256)| {
                let destination = tree.join("targets").join(file);
                let source = &source;
                async move {
                    source
                        .copy(&format!("targets/{file}"), &destination, length, sha256)
                        .await
                }
            })
            .buffer_unordered(DOWNLOADS)
            .try_for_each(|()| async { Ok(()) })
            .await?;
        keep_previous(&web, &tree, served.as_ref(), &release)?;

        // Verify the staged tree from the TUF root, as a new client would.
        let datastore = tempfile::tempdir_in(stage.path())?;
        let metadata = File::open(tree.join("metadata"))?;
        let targets = File::open(tree.join("targets"))?;
        let verified = RepositoryLoader::new(&anchor, fd_url(&metadata)?, fd_url(&targets)?)
            .limits(TUF_LIMITS)
            .datastore(datastore.path())
            .load()
            .await
            .context("verifying the staged replica")?;
        let summary = format!(
            "Replicated TUF timestamp version {} from {} to {}: {} new targets, valid until {}",
            verified.timestamp().signed.version,
            source.describe(),
            web.display(),
            fetched,
            verified.timestamp().signed.expires
        );
        anyhow::Ok((summary, release))
    })?;

    File::open(&tree)?.sync_all()?;
    let directory = File::open(&parent)?;
    let staged = Path::new(stage.path().file_name().context("missing stage name")?).join("tree");
    if web.exists() {
        renameat_with(
            &directory,
            &staged,
            &directory,
            &name,
            RenameFlags::EXCHANGE,
        )?;
        directory.sync_all()?;
        // Wait for local readers of the old tree, such as a downstream replica, to finish.
        let old = File::open(&tree)?;
        flock(&old, FlockOperation::LockExclusive)?;
    } else {
        renameat_with(
            &directory,
            &staged,
            &directory,
            &name,
            RenameFlags::NOREPLACE,
        )?;
        directory.sync_all()?;
    }
    // The swap is done. Without the record, the next run only hashes again.
    if let Err(error) = Verified::record(&record, &web, &release) {
        eprintln!("Warning: recording the verified targets: {error:#}");
    }
    Ok(summary)
}

/// Keep the previous release's files for clients that loaded it before the swap.
///
/// When the source publishes a new release, the previous one is the served
/// release. When it only re-signs the timestamp of the served release, the
/// previous one is still the release the served tree already keeps, so every
/// served file is carried forward.
fn keep_previous(
    web: &Path,
    tree: &Path,
    served: Option<&Release>,
    release: &Release,
) -> Result<()> {
    let Some(served) = served else {
        return Ok(());
    };
    let kept = if served.snapshot == release.snapshot {
        let mut kept = BTreeSet::new();
        for directory in ["metadata", "targets"] {
            for entry in fs::read_dir(web.join(directory))? {
                let entry = entry?;
                if entry.file_name() != "timestamp.json" && entry.file_type()?.is_file() {
                    kept.insert(Path::new(directory).join(entry.file_name()));
                }
            }
        }
        kept
    } else {
        served
            .metadata
            .keys()
            .map(|file| Path::new("metadata").join(file))
            .chain(
                served
                    .targets
                    .keys()
                    .map(|file| Path::new("targets").join(file)),
            )
            .collect()
    };
    for file in kept {
        let destination = tree.join(&file);
        if !destination.exists() && web.join(&file).is_file() {
            link_or_copy(&web.join(&file), &destination)?;
        }
    }
    Ok(())
}
