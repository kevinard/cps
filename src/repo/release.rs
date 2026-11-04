use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{DirBuilder, File, OpenOptions},
    io::{Read, Seek, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, TryStreamExt, pin_mut, stream};
use reqwest::{Client, StatusCode};
use rustix::fs::{RenameFlags, renameat_with};
use rustix::process::geteuid;
use tempfile::{NamedTempFile, TempDir};
use tough::{
    FilesystemTransport, Repository, RepositoryLoader, TargetName, Transport, TransportError,
    TransportErrorKind, TransportStream, async_trait,
};
use url::Url;

use crate::{
    image::Publisher,
    model::{Action, digest, digest_reader, validate_digest, validate_publisher},
    transport::https_client,
};

use super::{
    CATALOG_LIMIT, MANIFEST_LIMIT, Published, Root, Snapshot, catalog_snapshot, load_manifest,
    lock_repo, parse_current, path_present, prepare_objects, read_catalog, read_current, read_key,
    read_limited, read_pointer, write_atomic,
};

use crate::attestation::ATTESTATION_LIMIT;

const ROOT_LIMIT: u64 = 1024 * 1024;
const ROLE_LIMIT: u64 = 16 * 1024 * 1024;
const ENDPOINTS_LIMIT: u64 = 8192;
const PAYLOAD_DOWNLOADS: usize = 8;
const PUBLISHER_CHECKPOINT: &str = ".tuf/catalog";
const RETAINED: &str = "retained";
const MAX_RETAINED: usize = 32;
const RETAINED_LIMIT: u64 = (65 * MAX_RETAINED) as u64;
const CHECKPOINT_FILES: [(&str, u64); 4] = [
    ("root.json", ROOT_LIMIT),
    ("timestamp.json", ROOT_LIMIT),
    ("snapshot.json", ROLE_LIMIT),
    ("latest_known_time.json", 128),
];

#[derive(Clone, Debug)]
struct MirrorTransport(Option<Client>);

impl MirrorTransport {
    async fn open(&self, url: Url) -> std::result::Result<TransportStream, TransportError> {
        if url.scheme() == "file" {
            return FilesystemTransport.fetch(url).await;
        }
        let error = |kind| TransportError::new(kind, url.as_str());
        let client = self
            .0
            .as_ref()
            .filter(|_| url.scheme() == "https")
            .ok_or_else(|| error(TransportErrorKind::UnsupportedUrlScheme))?;
        let response = client.get(url.clone()).send().await.map_err(|err| {
            TransportError::new_with_cause(TransportErrorKind::Other, url.as_str(), err)
        })?;
        match response.status() {
            StatusCode::NOT_FOUND | StatusCode::GONE => {
                return Err(error(TransportErrorKind::FileNotFound));
            }
            StatusCode::OK => {}
            status => {
                return Err(TransportError::new_with_cause(
                    TransportErrorKind::Other,
                    url.as_str(),
                    std::io::Error::other(format!("HTTP status {status}")),
                ));
            }
        }
        Ok(Box::pin(response.bytes_stream().map_err(move |err| {
            TransportError::new_with_cause(TransportErrorKind::Other, url.as_str(), err)
        })))
    }
}

#[async_trait]
impl Transport for MirrorTransport {
    async fn fetch(&self, url: Url) -> std::result::Result<TransportStream, TransportError> {
        let metadata = url.path().ends_with(".json");
        let mut count = 0u64;
        // tough distinguishes a missing root from a failed root transfer by the stream error.
        let body = self
            .open(url.clone())
            .await
            .unwrap_or_else(|err| Box::pin(stream::iter([Err(err)])));
        Ok(Box::pin(body.map(move |chunk| {
            let chunk = chunk?;
            count = count.saturating_add(chunk.len() as u64);
            if metadata && count > ROLE_LIMIT {
                return Err(TransportError::new_with_cause(
                    TransportErrorKind::Other,
                    url.as_str(),
                    std::io::Error::other("TUF metadata exceeds the download limit"),
                ));
            }
            Ok(chunk)
        })))
    }
}

fn endpoint(value: &str) -> Result<Url> {
    let url = Url::parse(value)?;
    ensure!(
        matches!(url.scheme(), "file" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path().ends_with('/'),
        "mirror endpoint must be a directory URL with a trailing slash, using https:// or file://"
    );
    Ok(url)
}

struct Mirror {
    metadata: Url,
    targets: Url,
}

impl Mirror {
    fn new(metadata: &str, targets: &str) -> Result<Self> {
        Ok(Self {
            metadata: endpoint(metadata)?,
            targets: endpoint(targets)?,
        })
    }

    fn read(state: &Root) -> Result<Self> {
        let config = String::from_utf8(read_limited(
            state
                .file("endpoints")
                .context("run repo sync to configure this cache's mirror")?,
            ENDPOINTS_LIMIT,
        )?)?;
        let mut lines = config.lines();
        let (Some(metadata), Some(targets), None) = (lines.next(), lines.next(), lines.next())
        else {
            anyhow::bail!("invalid cached mirror endpoints");
        };
        Self::new(metadata, targets)
    }

    fn document(&self) -> String {
        format!("{}\n{}\n", self.metadata, self.targets)
    }
}

fn open_state(repo: &Path) -> Result<Root> {
    let root = Root::open(repo)?.subdir(".tuf")?;
    let permissions = root.as_file().metadata()?;
    ensure!(
        permissions.uid() == geteuid().as_raw() && permissions.mode() & 0o077 == 0,
        "TUF state directory must be private to the sync account"
    );
    Ok(root)
}

fn copy_checkpoint(from: &Path, to: &Path) -> Result<()> {
    let root = Root::open(from)?;
    for (name, limit) in CHECKPOINT_FILES {
        if path_present(&from.join(name))? {
            let bytes = read_limited(root.file(name)?, limit)?;
            // A failed library write must never replace a durable checkpoint with partial JSON.
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .with_context(|| format!("invalid TUF checkpoint file {name}"))?;
            let mut file = File::create(to.join(name))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
    }
    File::open(to)?.sync_all()?;
    Ok(())
}

fn checkpoint(state: &Path, scratch: &Path, trusted_exists: bool) -> Result<()> {
    let next = TempDir::new_in(state)?;
    copy_checkpoint(scratch, next.path())?;
    let parent = File::open(state)?;
    let name = next.path().file_name().context("missing checkpoint name")?;
    renameat_with(
        &parent,
        name,
        &parent,
        "trusted",
        if trusted_exists {
            RenameFlags::EXCHANGE
        } else {
            RenameFlags::NOREPLACE
        },
    )?;
    parent.sync_all()?;
    Ok(())
}

/// Verify TUF metadata and materialize the reachable CPS release in a local cache.
pub fn sync(
    repo: &Path,
    trusted_root: &Path,
    metadata_url: &str,
    targets_url: &str,
    publisher: &str,
    trust_key: &Path,
) -> Result<String> {
    synchronize(
        repo,
        trusted_root,
        Mirror::new(metadata_url, targets_url)?,
        publisher,
        trust_key,
        true,
    )
}

/// Verify and cache release metadata, leaving payloads for plan-specific fetches.
pub fn sync_index(
    repo: &Path,
    trusted_root: &Path,
    metadata_url: &str,
    targets_url: &str,
    publisher: &str,
    trust_key: &Path,
) -> Result<String> {
    synchronize(
        repo,
        trusted_root,
        Mirror::new(metadata_url, targets_url)?,
        publisher,
        trust_key,
        false,
    )
}

fn synchronize(
    repo: &Path,
    trusted_root: &Path,
    mirror: Mirror,
    publisher: &str,
    trust_key: &Path,
    full: bool,
) -> Result<String> {
    validate_publisher(publisher)?;
    let key = read_key(trust_key)?;
    let anchor = read_limited(File::open(trusted_root)?, ROOT_LIMIT)
        .with_context(|| format!("reading trusted TUF root {}", trusted_root.display()))?;
    let endpoints = mirror.document();
    ensure!(
        endpoints.len() as u64 <= ENDPOINTS_LIMIT,
        "mirror endpoints exceed the size limit"
    );
    let _lock = lock_repo(repo)?;
    let state = repo.join(".tuf");
    let origin = format!("{}\n{publisher}\n{}\n", digest(&anchor), hex::encode(&key));
    let published = path_present(&repo.join("current"))?;
    if !path_present(&state)? {
        ensure!(!published, "TUF sync requires a new repository cache");
        DirBuilder::new().mode(0o700).create(&state)?;
        File::open(repo)?.sync_all()?;
    }
    let directory = open_state(repo)?;
    let trusted_exists = path_present(&state.join("trusted"))?;
    if path_present(&state.join("origin"))? {
        ensure!(
            read_limited(directory.file("origin")?, origin.len() as u64)? == origin.as_bytes(),
            "TUF cache was initialized with a different trust root or publisher"
        );
    } else {
        ensure!(
            !published && !trusted_exists,
            "TUF cache is missing its trust configuration"
        );
        write_atomic(&state.join("origin"), origin.as_bytes(), true)?;
    }
    ensure!(
        !published || trusted_exists,
        "TUF cache is missing its trusted metadata"
    );

    let scratch = TempDir::new_in(&state)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let hash = runtime.block_on(async {
        let verified =
            load_mirror(&state, &directory, scratch.path(), mirror, Some(anchor)).await?;
        prepare_objects(repo)?;
        materialize(repo, &verified, publisher, &key, full).await
    })?;
    write_atomic(&state.join("endpoints"), endpoints.as_bytes(), false)?;
    write_atomic(&repo.join("current"), format!("{hash}\n").as_bytes(), false)?;
    Ok(hash)
}

async fn load_mirror(
    state: &Path,
    directory: &Root,
    scratch: &Path,
    mirror: Mirror,
    anchor: Option<Vec<u8>>,
) -> Result<Repository> {
    let transport = MirrorTransport(
        (mirror.metadata.scheme() == "https" || mirror.targets.scheme() == "https")
            .then(|| https_client().build())
            .transpose()?,
    );
    let trusted = state.join("trusted");
    let trusted_exists = path_present(&trusted)?;
    let root = if trusted_exists {
        directory.dir("trusted")?;
        copy_checkpoint(&trusted, scratch)?;
        read_limited(directory.file("trusted/root.json")?, ROOT_LIMIT)?
    } else {
        anchor.context("TUF cache is missing its trusted metadata")?
    };
    let result = RepositoryLoader::new(&root, mirror.metadata, mirror.targets)
        .transport(transport)
        .datastore(scratch)
        .load()
        .await;
    // The library writes root, timestamp and snapshot only after verifying each role.
    // Retain that progress even when a later role or the release contents fail to download.
    if path_present(&scratch.join("root.json"))? {
        checkpoint(state, scratch, trusted_exists)?;
    }
    result.context("verifying TUF metadata")
}

pub(crate) fn fetch_payloads(publisher: &Publisher, blobs: BTreeMap<&str, u64>) -> Result<()> {
    let repo = &publisher.repo;
    let mut missing = Vec::new();
    for (hash, length) in blobs {
        validate_digest(hash)?;
        if !cached_object(repo, &format!("blobs/{hash}"), hash, length)? {
            missing.push((hash, length));
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let state = repo.join(".tuf");
    ensure!(
        path_present(&state)?,
        "payload {} is missing or damaged and the repository has no TUF mirror",
        missing[0].0
    );
    let _lock = lock_repo(repo)?;
    let directory = open_state(repo)?;
    let identity = format!("\n{}\n{}\n", publisher.name, hex::encode(&publisher.key));
    let origin = String::from_utf8(read_limited(
        directory.file("origin")?,
        64 + identity.len() as u64,
    )?)?;
    let anchor = origin
        .strip_suffix(&identity)
        .context("TUF cache is configured for a different publisher or key")?;
    validate_digest(anchor)?;
    let mirror = Mirror::read(&directory)?;
    let scratch = TempDir::new_in(&state)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let verified = load_mirror(&state, &directory, scratch.path(), mirror, None).await?;
        stream::iter(missing)
            .map(|(hash, length)| {
                fetch_object(&verified, repo, "blobs", hash, length, Some(length))
            })
            .buffer_unordered(PAYLOAD_DOWNLOADS)
            .try_for_each(|()| async { Ok(()) })
            .await
    })
}

async fn materialize(
    repo: &Path,
    verified: &Repository,
    publisher: &str,
    key: &[u8],
    full: bool,
) -> Result<String> {
    let pointer = download(verified, "current", repo, 65, None, None)
        .await?
        .context("missing catalog pointer download")?;
    let hash = parse_current(&String::from_utf8(read_limited(pointer.reopen()?, 65)?)?)?;
    fetch_object(verified, repo, "catalogs", &hash, CATALOG_LIMIT, None).await?;
    let catalog = read_catalog(repo, &hash, publisher, key)?;
    let checkpoint = repo.join(PUBLISHER_CHECKPOINT);
    let trusted = path_present(&checkpoint)?
        .then(|| read_pointer(repo, PUBLISHER_CHECKPOINT))
        .transpose()?;
    if trusted.as_deref() != Some(&hash) {
        let previous = match trusted {
            Some(previous) => Some(previous),
            None if path_present(&repo.join("current"))? => Some(read_current(repo)?),
            None => None,
        };
        if let Some(previous) = previous.filter(|previous| previous != &hash) {
            read_catalog(repo, &previous, publisher, key)?.check_successor(&catalog, &previous)?;
        }
        // Remember the signed catalog even when its payloads cannot be fetched.
        write_atomic(&checkpoint, format!("{hash}\n").as_bytes(), false)?;
    }
    let mut entries = BTreeMap::new();
    let mut attestations = BTreeSet::new();
    if verified
        .targets()
        .signed
        .targets
        .contains_key(&TargetName::new(RETAINED)?)
    {
        let file = download(verified, RETAINED, repo, RETAINED_LIMIT, None, None)
            .await?
            .context("missing retained catalog list download")?;
        let document = String::from_utf8(read_limited(file.reopen()?, RETAINED_LIMIT)?)?;
        let retained = retained_catalogs(document.lines(), &hash)?;
        for pin in retained {
            fetch_object(verified, repo, "catalogs", pin, CATALOG_LIMIT, None).await?;
            let previous = read_catalog(repo, pin, publisher, key)?;
            previous.check_successor(&catalog, pin).with_context(|| {
                format!("checking retained catalog {pin} against current {hash}")
            })?;
            entries.extend(previous.entries);
            attestations.extend(previous.attestations.into_values());
        }
    }
    entries.extend(catalog.entries);
    attestations.extend(catalog.attestations.into_values());
    cache_objects(verified, repo, publisher, entries, attestations, full).await?;
    Ok(hash)
}

fn retained_catalogs<'a>(
    pins: impl IntoIterator<Item = &'a str>,
    current: &str,
) -> Result<BTreeSet<&'a str>> {
    let mut retained = BTreeSet::new();
    for pin in pins {
        validate_digest(pin)?;
        ensure!(pin != current, "current catalog cannot also be retained");
        ensure!(retained.insert(pin), "duplicate retained catalog {pin}");
        ensure!(retained.len() <= MAX_RETAINED, "too many retained catalogs");
    }
    Ok(retained)
}

async fn cache_objects(
    verified: &Repository,
    repo: &Path,
    publisher: &str,
    entries: BTreeMap<String, String>,
    attestations: BTreeSet<String>,
    full: bool,
) -> Result<()> {
    let mut packages = Vec::with_capacity(entries.len());
    for (id, sha256) in entries {
        fetch_object(verified, repo, "manifests", &sha256, MANIFEST_LIMIT, None).await?;
        let package = load_manifest(repo, sha256)?;
        ensure!(
            package.manifest.id() == id && package.manifest.publisher == publisher,
            "manifest identity mismatch for {id}"
        );
        packages.push(package);
    }
    for (sha256, length) in release_blobs(&packages)? {
        if full {
            fetch_object(verified, repo, "blobs", sha256, length, Some(length)).await?;
        } else {
            checked_target(
                verified,
                &format!("blobs/{sha256}"),
                length,
                Some(sha256),
                Some(length),
            )?;
        }
    }
    for sha256 in attestations {
        fetch_object(
            verified,
            repo,
            "attestations",
            &sha256,
            ATTESTATION_LIMIT,
            None,
        )
        .await?;
    }
    Ok(())
}

fn release_blobs(packages: &[Published]) -> Result<BTreeMap<&str, u64>> {
    let mut blobs = BTreeMap::new();
    for package in packages {
        for action in &package.manifest.action {
            if let Action::File {
                sha256: Some(hash),
                size: Some(length),
                ..
            } = action
            {
                let saved = blobs.entry(hash.as_str()).or_insert(*length);
                ensure!(*saved == *length, "conflicting payload lengths for {hash}");
            }
        }
    }
    Ok(blobs)
}

async fn fetch_object(
    verified: &Repository,
    repo: &Path,
    kind: &str,
    hash: &str,
    limit: u64,
    length: Option<u64>,
) -> Result<()> {
    validate_digest(hash)?;
    let name = format!("{kind}/{hash}");
    if let Some(file) = download(verified, &name, repo, limit, Some(hash), length).await? {
        let path = repo.join(name);
        // A verified download may repair a corrupted object at this digest.
        file.persist(&path)?;
        File::open(path.parent().context("missing object directory")?)?.sync_all()?;
    }
    Ok(())
}

fn checked_target<'a>(
    verified: &'a Repository,
    name: &str,
    limit: u64,
    hash: Option<&str>,
    length: Option<u64>,
) -> Result<(TargetName, &'a tough::schema::Target)> {
    let target_name = TargetName::new(name.replace('/', "-"))?;
    let target = verified
        .targets()
        .signed
        .find_target(&target_name, false)
        .with_context(|| format!("checking TUF target {name}"))?;
    ensure!(
        target.length <= limit && length.is_none_or(|length| target.length == length),
        "invalid signed length for {name}"
    );
    let signed_hash = hex::encode(target.hashes.sha256.as_ref());
    ensure!(
        hash.is_none_or(|hash| hash == signed_hash),
        "signed digest mismatch for {name}"
    );
    Ok((target_name, target))
}

fn cached_object(repo: &Path, name: &str, hash: &str, length: u64) -> Result<bool> {
    if !path_present(&repo.join(name))? {
        return Ok(false);
    }
    let existing = Root::open(repo)?.file(name)?;
    Ok(existing.metadata()?.len() == length
        && digest_reader(existing.take(length.saturating_add(1)))? == hash)
}

async fn download(
    verified: &Repository,
    name: &str,
    repo: &Path,
    limit: u64,
    hash: Option<&str>,
    length: Option<u64>,
) -> Result<Option<NamedTempFile>> {
    let (target_name, target) = checked_target(verified, name, limit, hash, length)?;
    if let Some(hash) = hash
        && cached_object(repo, name, hash, target.length)?
    {
        return Ok(None);
    }

    let parent = Path::new(name).parent().context("missing target parent")?;
    let mut file = NamedTempFile::new_in(repo.join(parent))?;
    let stream = verified
        .read_target(&target_name)
        .await?
        .with_context(|| format!("missing TUF target {name}"))?;
    pin_mut!(stream);
    let mut written = 0u64;
    while let Some(bytes) = stream
        .try_next()
        .await
        .with_context(|| format!("reading target {name}"))?
    {
        file.write_all(&bytes)?;
        written += bytes.len() as u64;
    }
    ensure!(written == target.length, "short download for {name}");
    file.as_file().sync_all()?;
    Ok(Some(file))
}

/// Export the signed catalog and its reachable objects as a tree of TUF targets.
pub fn export(repo: &Path, publisher: &str, trust_key: &Path, output: &Path) -> Result<String> {
    export_with_retained(repo, publisher, trust_key, output, &[])
}

/// Include retained signed catalogs and their objects in the current TUF target set.
pub fn export_with_retained(
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    output: &Path,
    retained: &[String],
) -> Result<String> {
    let key = read_key(trust_key)?;
    let current = read_current(repo)?;
    let retained = retained_catalogs(retained.iter().map(String::as_str), &current)?;
    let catalog = read_catalog(repo, &current, publisher, &key)?;
    let root = Root::open(repo)?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stage = TempDir::new_in(parent)?;
    for pin in &retained {
        let previous = read_catalog(repo, pin, publisher, &key)?;
        previous.check_successor(&catalog, pin).with_context(|| {
            format!("checking retained catalog {pin} against current {current}")
        })?;
        let release = catalog_snapshot(repo, previous, (*pin).to_owned(), &key)?;
        export_catalog(&root, stage.path(), &release)?;
    }
    let release = catalog_snapshot(repo, catalog, current.clone(), &key)?;
    export_catalog(&root, stage.path(), &release)?;
    if !retained.is_empty() {
        let mut document = String::new();
        for pin in retained {
            document.push_str(pin);
            document.push('\n');
        }
        write_atomic(&stage.path().join(RETAINED), document.as_bytes(), false)?;
    }
    write_atomic(
        &stage.path().join("current"),
        format!("{current}\n").as_bytes(),
        false,
    )?;

    let directory = File::open(parent)?;
    renameat_with(
        &directory,
        stage
            .path()
            .file_name()
            .context("missing export staging name")?,
        &directory,
        output.file_name().context("missing export name")?,
        RenameFlags::NOREPLACE,
    )?;
    directory.sync_all()?;
    Ok(current)
}

fn export_catalog(root: &Root, output: &Path, release: &Snapshot) -> Result<()> {
    export_object(
        root,
        output,
        "catalogs",
        &release.sha256,
        CATALOG_LIMIT,
        None,
    )?;
    for package in &release.packages {
        export_object(
            root,
            output,
            "manifests",
            &package.sha256,
            MANIFEST_LIMIT,
            None,
        )?;
    }
    for (hash, length) in release_blobs(&release.packages)? {
        export_object(root, output, "blobs", hash, length, Some(length))?;
    }
    for hash in release.builds.attestations.values() {
        export_object(root, output, "attestations", hash, ATTESTATION_LIMIT, None)?;
    }
    Ok(())
}

fn export_object(
    root: &Root,
    output: &Path,
    kind: &str,
    hash: &str,
    limit: u64,
    length: Option<u64>,
) -> Result<()> {
    let name = format!("{kind}/{hash}");
    let destination = output.join(format!("{kind}-{hash}"));
    if path_present(&destination)? {
        let size = std::fs::metadata(destination)?.len();
        ensure!(
            size <= limit && length.is_none_or(|length| size == length),
            "conflicting object length for {name}"
        );
        return Ok(());
    }
    let source = root.file(&name)?;
    let mut dest = OpenOptions::new()
        .create_new(true)
        .write(true)
        .read(true)
        .open(destination)?;
    let copied = std::io::copy(&mut source.take(limit.saturating_add(1)), &mut dest)?;
    dest.rewind()?;
    ensure!(
        copied <= limit
            && length.is_none_or(|length| copied == length)
            && digest_reader(&mut dest)? == hash,
        "corrupt object {name}"
    );
    dest.sync_all()?;
    Ok(())
}
