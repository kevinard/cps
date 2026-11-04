use std::{
    collections::BTreeMap,
    fs::{DirBuilder, File, OpenOptions},
    io::{Read, Seek, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, TryStreamExt, pin_mut, stream};
use reqwest::{Client, StatusCode, redirect::Policy};
use rustix::fs::{RenameFlags, renameat_with};
use rustix::process::geteuid;
use tempfile::{NamedTempFile, TempDir};
use tough::{
    FilesystemTransport, Repository, RepositoryLoader, TargetName, Transport, TransportError,
    TransportErrorKind, TransportStream, async_trait,
};
use url::Url;

use crate::model::{Action, digest, digest_reader, validate_digest, validate_publisher};

use super::{
    CATALOG_LIMIT, MANIFEST_LIMIT, Published, Root, load_manifest, lock_repo, parse_current,
    path_present, prepare_objects, read_catalog, read_key, read_limited, snapshot, write_atomic,
};

use crate::attestation::ATTESTATION_LIMIT;

const ROOT_LIMIT: u64 = 1024 * 1024;
const ROLE_LIMIT: u64 = 16 * 1024 * 1024;
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
    let metadata = endpoint(metadata_url)?;
    let targets = endpoint(targets_url)?;
    validate_publisher(publisher)?;
    let key = read_key(trust_key)?;
    let anchor = read_limited(File::open(trusted_root)?, ROOT_LIMIT)
        .with_context(|| format!("reading trusted TUF root {}", trusted_root.display()))?;
    let transport = MirrorTransport(
        (metadata.scheme() == "https" || targets.scheme() == "https")
            .then(|| {
                Client::builder()
                    .https_only(true)
                    .redirect(Policy::none())
                    .connect_timeout(Duration::from_secs(10))
                    .read_timeout(Duration::from_secs(30))
                    .build()
            })
            .transpose()?,
    );
    let _lock = lock_repo(repo)?;
    let directory = Root::open(repo)?;
    let state = repo.join(".tuf");
    let trusted = state.join("trusted");
    let origin = format!("{}\n{publisher}\n{}\n", digest(&anchor), hex::encode(&key));
    let published = path_present(&repo.join("current"))?;
    if !path_present(&state)? {
        ensure!(!published, "TUF sync requires a new repository cache");
        DirBuilder::new().mode(0o700).create(&state)?;
        File::open(repo)?.sync_all()?;
    }
    let permissions = directory.dir(".tuf")?.metadata()?;
    ensure!(
        permissions.uid() == geteuid().as_raw() && permissions.mode() & 0o077 == 0,
        "TUF state directory must be private to the sync account"
    );
    let trusted_exists = path_present(&trusted)?;
    if path_present(&state.join("origin"))? {
        ensure!(
            read_limited(directory.file(".tuf/origin")?, origin.len() as u64)? == origin.as_bytes(),
            "TUF cache was initialized with a different trust root or publisher"
        );
    } else {
        ensure!(
            !published && !trusted_exists,
            "TUF cache is missing its trust configuration"
        );
        write_atomic(&state.join("origin"), origin.as_bytes(), true)?;
    }

    let scratch = TempDir::new_in(&state)?;
    let root = if trusted_exists {
        directory.dir(".tuf/trusted")?;
        copy_checkpoint(&trusted, scratch.path())?;
        read_limited(directory.file(".tuf/trusted/root.json")?, ROOT_LIMIT)?
    } else {
        ensure!(!published, "TUF cache is missing its trusted metadata");
        anchor
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(
        RepositoryLoader::new(&root, metadata, targets)
            .transport(transport)
            .datastore(scratch.path())
            .load(),
    );
    // The library writes root, timestamp and snapshot only after verifying each role.
    // Retain that progress even when a later role or the release contents fail to download.
    if path_present(&scratch.path().join("root.json"))? {
        checkpoint(&state, scratch.path(), trusted_exists)?;
    }
    let verified = result.context("verifying TUF metadata")?;
    prepare_objects(repo)?;
    runtime.block_on(materialize(repo, &verified, publisher, &key))
}

async fn materialize(
    repo: &Path,
    verified: &Repository,
    publisher: &str,
    key: &[u8],
) -> Result<String> {
    let pointer = download(verified, "current", repo, 65, None, None)
        .await?
        .context("missing catalog pointer download")?;
    let hash = parse_current(&String::from_utf8(read_limited(pointer.reopen()?, 65)?)?)?;
    fetch_object(verified, repo, "catalogs", &hash, CATALOG_LIMIT, None).await?;
    let catalog = read_catalog(repo, &hash, publisher, key)?;
    let mut packages = Vec::with_capacity(catalog.entries.len());
    for (id, sha256) in catalog.entries {
        fetch_object(verified, repo, "manifests", &sha256, MANIFEST_LIMIT, None).await?;
        let package = load_manifest(repo, sha256)?;
        ensure!(
            package.manifest.id() == id && package.manifest.publisher == publisher,
            "manifest identity mismatch for {id}"
        );
        packages.push(package);
    }
    for (sha256, length) in release_blobs(&packages)? {
        fetch_object(verified, repo, "blobs", sha256, length, Some(length)).await?;
    }
    for sha256 in catalog.attestations.values() {
        fetch_object(
            verified,
            repo,
            "attestations",
            sha256,
            ATTESTATION_LIMIT,
            None,
        )
        .await?;
    }
    write_atomic(&repo.join("current"), format!("{hash}\n").as_bytes(), false)?;
    Ok(hash)
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
        file.persist_noclobber(&path)?;
        File::open(path.parent().context("missing object directory")?)?.sync_all()?;
    }
    Ok(())
}

async fn download(
    verified: &Repository,
    name: &str,
    repo: &Path,
    limit: u64,
    hash: Option<&str>,
    length: Option<u64>,
) -> Result<Option<NamedTempFile>> {
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
    if hash.is_some() && path_present(&repo.join(name))? {
        let existing = Root::open(repo)?.file(name)?;
        ensure!(
            existing.metadata()?.len() == target.length
                && digest_reader(existing.take(target.length.saturating_add(1)))? == signed_hash,
            "corrupt cached object {name}"
        );
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
    let release = snapshot(repo, publisher, &read_key(trust_key)?, None)?;
    let root = Root::open(repo)?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stage = TempDir::new_in(parent)?;
    export_object(
        &root,
        stage.path(),
        "catalogs",
        &release.sha256,
        CATALOG_LIMIT,
        None,
    )?;
    for package in &release.packages {
        export_object(
            &root,
            stage.path(),
            "manifests",
            &package.sha256,
            MANIFEST_LIMIT,
            None,
        )?;
    }
    for (hash, length) in release_blobs(&release.packages)? {
        export_object(&root, stage.path(), "blobs", hash, length, Some(length))?;
    }
    for hash in release.attestations.values() {
        export_object(
            &root,
            stage.path(),
            "attestations",
            hash,
            ATTESTATION_LIMIT,
            None,
        )?;
    }
    write_atomic(
        &stage.path().join("current"),
        format!("{}\n", release.sha256).as_bytes(),
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
    Ok(release.sha256)
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
    let source = root.file(&name)?;
    let mut dest = OpenOptions::new()
        .create_new(true)
        .write(true)
        .read(true)
        .open(output.join(format!("{kind}-{hash}")))?;
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
