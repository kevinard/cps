use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions, Permissions},
    io::{Read, Seek, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
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
    build::sources::publish_verified,
    image::{Image, Publisher, State},
    model::{Action, FileDigest, digest, digest_reader, validate_digest, validate_publisher},
    transport::https_client,
};

use super::{
    CATALOG_LIMIT, Catalog, CatalogEntry, CatalogPin, FILE_INDEX_LIMIT, FILE_INDEX_OBJECTS,
    MANIFEST_LIMIT, Published, Root, Snapshot, catalog_snapshot, lock_repo, parse_current,
    parse_public_key, path_present, prepare_objects, read_catalog, read_current, read_key,
    read_key_der, read_limited, read_package, read_pointer, rebuild, source_archive, write_atomic,
};

use crate::attestation::ATTESTATION_LIMIT;

const ROOT_LIMIT: u64 = 1024 * 1024;
const PUBLISHER_NAME_TARGET: &str = "publisher";
const PUBLISHER_KEY_TARGET: &str = "publisher.der";
const ROLE_LIMIT: u64 = 16 * 1024 * 1024;
const ENDPOINTS_LIMIT: u64 = 8192;
const METADATA_DOWNLOADS: usize = 8;
const PAYLOAD_DOWNLOADS: usize = 8;
const PAYLOAD_ZSTD_LEVEL: i32 = 3;
const PAYLOAD_ZSTD_WINDOW_LOG: u32 = 21;
const CACHE_FORMAT: &str = "cps mirror cache v1\n";
const PUBLISHER_CHECKPOINT: &str = ".tuf/catalog";
const PROFILE_BINDING: &str = "profile";
const REFRESH_ROOT: &str = "refresh-root.json";
const ARCHIVE: &str = "archive";
const RETAINED: &str = "retained";
const MAX_RETAINED: usize = 32;
const RETAINED_LIMIT: u64 = (65 * MAX_RETAINED) as u64;

mod sign;
pub use sign::{SignExport, sign_export};
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

mod archive;
mod bootstrap;
mod cache;
mod delta;
mod profile;
pub(crate) use archive::ArchiveCache;
pub use bootstrap::PublisherBootstrap;
pub use cache::{
    CacheRetention, PruneReport, RetainedRoot, prune_payloads, prune_payloads_with_locks,
};
pub(crate) use cache::{prune_system_payloads, prune_with_registered_roots};
pub use profile::{
    ProfilePayloads, sync_profile, sync_profile_for_image, sync_profile_with_payloads,
};

#[derive(Clone)]
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

    fn from_base(base: &str) -> Result<Self> {
        Self::from_base_url(&mirror_base(base)?)
    }

    fn from_base_url(base: &Url) -> Result<Self> {
        Self::new(
            base.join("metadata/")?.as_str(),
            base.join("targets/")?.as_str(),
        )
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

fn mirror_base(base: &str) -> Result<Url> {
    if base.ends_with('/') {
        endpoint(base)
    } else {
        endpoint(&format!("{base}/"))
    }
}

fn read_tuf_root(path: &Path) -> Result<Vec<u8>> {
    (|| read_limited(File::open(path)?, ROOT_LIMIT))()
        .with_context(|| format!("reading trusted TUF root {}", path.display()))
}

fn open_state(repo: &Path) -> Result<Root> {
    let root = Root::open(repo)?.subdir(".tuf")?;
    let permissions = root.as_file().metadata()?;
    ensure!(
        permissions.uid() == geteuid().as_raw() && permissions.mode() & 0o077 == 0,
        "TUF state directory must be private to the sync account"
    );
    let mut version = [0; CACHE_FORMAT.len()];
    root.file("origin")
        .context("TUF cache is missing its trust configuration")?
        .read_exact(&mut version)
        .context("reading TUF cache format")?;
    ensure!(
        version == CACHE_FORMAT.as_bytes(),
        "unsupported TUF cache format"
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
    sync_with_options(
        repo,
        trusted_root,
        metadata_url,
        targets_url,
        publisher,
        trust_key,
        SyncOptions::default(),
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
    sync_with_options(
        repo,
        trusted_root,
        metadata_url,
        targets_url,
        publisher,
        trust_key,
        SyncOptions {
            index_only: true,
            image: None,
        },
    )
}

#[derive(Default)]
pub struct SyncOptions<'a> {
    pub index_only: bool,
    pub image: Option<&'a Path>,
}

pub fn sync_with_options(
    repo: &Path,
    trusted_root: &Path,
    metadata_url: &str,
    targets_url: &str,
    publisher: &str,
    trust_key: &Path,
    options: SyncOptions<'_>,
) -> Result<String> {
    sync_with_required_catalogs(
        repo,
        trusted_root,
        metadata_url,
        targets_url,
        publisher,
        trust_key,
        options,
        &[],
    )
}

/// Sync only if the authenticated mirror also advertises all the requested catalog pins.
#[allow(clippy::too_many_arguments)]
pub fn sync_with_required_catalogs(
    repo: &Path,
    trusted_root: &Path,
    metadata_url: &str,
    targets_url: &str,
    publisher: &str,
    trust_key: &Path,
    options: SyncOptions<'_>,
    required: &[String],
) -> Result<String> {
    ensure!(
        required.len() <= MAX_RETAINED + 1,
        "too many required catalogs"
    );
    let required = required
        .iter()
        .map(|hash| -> Result<_> {
            validate_digest(hash)?;
            Ok(CatalogPin::primary(publisher.to_owned(), hash.clone()))
        })
        .collect::<Result<Vec<_>>>()?;
    let pins = required.iter().collect::<Vec<_>>();
    let source = SyncSource::new(
        trusted_root,
        Mirror::new(metadata_url, targets_url)?,
        publisher,
        trust_key,
    )?;
    let guard = options.image.map(ImageGuard::open).transpose()?;
    if let Some(guard) = &guard {
        ensure!(
            guard.state.publishers.release_profile().is_none()
                || guard.state.publishers.is_local(publisher),
            "use repo sync-profile --image for an image that follows a signed profile"
        );
        guard.check_source(&source)?;
    }
    let _lock = lock_repo(repo)?;
    sync_current_locked(repo, &source, !options.index_only, &pins, guard.as_ref())
}

fn sync_current_locked(
    repo: &Path,
    source: &SyncSource<'_>,
    full: bool,
    required: &[&CatalogPin],
    guard: Option<&ImageGuard>,
) -> Result<String> {
    let (release, _) = stage_release(
        repo,
        source,
        full,
        SyncSelection::Current {
            binding: None,
            required,
            guard,
            defer_checkpoint: false,
        },
    )?;
    source.promote(repo, &release, None)?;
    Ok(release.current)
}

/// Prepare an independent mirror cache and record its original root for refreshes.
pub(crate) fn sync_url_publisher(
    repo: &Path,
    base: &str,
    trusted_root: &Path,
    publisher: &str,
    trust_key: &Path,
) -> Result<()> {
    let source = SyncSource::new(trusted_root, Mirror::from_base(base)?, publisher, trust_key)?;
    let _lock = lock_repo(repo)?;
    if path_present(&repo.join("current"))? {
        let directory =
            open_state(repo).context("use a new directory or a synchronized TUF cache")?;
        ensure!(
            Mirror::read(&directory)?.document() == source.mirror.document(),
            "the cache is configured for a different mirror URL"
        );
    }
    let refresh_root = repo.join(".tuf").join(REFRESH_ROOT);
    if path_present(&refresh_root)? {
        let directory = open_state(repo)?;
        ensure!(
            read_limited(directory.file(REFRESH_ROOT)?, ROOT_LIMIT)? == source.anchor,
            "the cache is configured for a different TUF root"
        );
    }
    sync_current_locked(repo, &source, false, &[], None)?;
    write_atomic(&refresh_root, &source.anchor, true)
}

pub(crate) fn refresh_url_publishers(
    root: &Path,
    required: &BTreeSet<&str>,
    base_only: bool,
    refresh: bool,
) -> Result<BTreeSet<String>> {
    let guard = ImageGuard::open(root)?;
    let publishers = &guard.state.publishers;
    let mut deferred = BTreeSet::new();
    // Profile sync alone controls the primary and companion catalogs.
    for (index, publisher) in publishers
        .as_slice()
        .iter()
        .enumerate()
        .skip(publishers.signed_publishers().len())
    {
        let required = index == 0 || required.contains(publisher.name.as_str());
        if base_only && !required {
            deferred.insert(publisher.name.clone());
            continue;
        }
        if !refresh {
            continue;
        }
        let repo = &publisher.repo;
        let sync = || -> Result<()> {
            if !path_present(&repo.join(".tuf").join(REFRESH_ROOT))? {
                ensure!(
                    path_present(&repo.join("current"))?,
                    "publisher has no cached catalog"
                );
                return Ok(());
            }
            eprintln!("Refreshing publisher {}…", publisher.name);
            let _lock = lock_repo(repo)?;
            let directory = open_state(repo)?;
            let source = SyncSource {
                publisher: &publisher.name,
                key: publisher.key.clone(),
                anchor: read_limited(directory.file(REFRESH_ROOT)?, ROOT_LIMIT)?,
                mirror: Mirror::read(&directory)?,
            };
            sync_current_locked(repo, &source, false, &[], Some(&guard))?;
            Ok(())
        };
        if let Err(error) =
            sync().with_context(|| format!("refreshing publisher {}", publisher.name))
        {
            if required {
                return Err(error);
            }
            eprintln!(
                "Warning: {error:#}. Deferring updates from {}.",
                publisher.name
            );
            deferred.insert(publisher.name.clone());
        }
    }
    Ok(deferred)
}

pub struct ArchiveSyncOptions<'a> {
    pub catalog: &'a str,
    pub index_only: bool,
}

/// Cache one exact catalog from a separate, authenticated archive mirror.
pub fn sync_archive(
    repo: &Path,
    trusted_root: &Path,
    metadata_url: &str,
    targets_url: &str,
    publisher: &str,
    trust_key: &Path,
    options: ArchiveSyncOptions<'_>,
) -> Result<String> {
    validate_digest(options.catalog)?;
    let source = SyncSource::new(
        trusted_root,
        Mirror::new(metadata_url, targets_url)?,
        publisher,
        trust_key,
    )?;
    let _lock = lock_repo(repo)?;
    stage_release(
        repo,
        &source,
        !options.index_only,
        SyncSelection::Archive(options.catalog),
    )?;
    write_atomic(
        &repo.join(".tuf/endpoints"),
        source.mirror.document().as_bytes(),
        false,
    )?;
    Ok(options.catalog.to_owned())
}

struct ImageGuard {
    image: Image,
    state: State,
}

impl ImageGuard {
    fn open(root: &Path) -> Result<Self> {
        let image =
            Image::open_query(root).context("opening the image used to guard mirror sync")?;
        image.require_clean()?;
        let state = image.state()?;
        Ok(Self { image, state })
    }

    fn check_source(&self, source: &SyncSource<'_>) -> Result<&Publisher> {
        let publisher = self
            .state
            .publishers
            .get(source.publisher)
            .with_context(|| format!("image has no publisher {}", source.publisher))?;
        ensure!(
            publisher.key == source.key,
            "mirror trust key differs from the image trust key for {}",
            source.publisher
        );
        Ok(publisher)
    }

    fn check(
        &self,
        repo: &Path,
        source: &SyncSource<'_>,
        hash: &str,
        catalog: &Catalog,
    ) -> Result<()> {
        let mut publisher = self.check_source(source)?.clone();
        publisher.repo = repo.to_owned();
        self.image
            .check_repository_candidate(
                &self.state,
                &publisher,
                catalog,
                &catalog.checkpoint(hash.to_owned()),
            )
            .with_context(|| {
                format!(
                    "checking mirror catalog against the installed image for {}",
                    source.publisher
                )
            })
    }
}

struct StagedRelease {
    current: String,
    retained: Vec<String>,
}

struct StagedMirror {
    verified: Repository,
    _scratch: TempDir,
    runtime: tokio::runtime::Runtime,
}

impl StagedMirror {
    fn fetch_payloads(&self, repo: &Path, blobs: &BTreeMap<String, u64>) -> Result<()> {
        self.runtime.block_on(fetch_verified_payloads(
            &self.verified,
            repo,
            blobs.iter().map(|(hash, length)| (hash.as_str(), *length)),
        ))
    }
}

struct SyncSource<'a> {
    publisher: &'a str,
    key: Vec<u8>,
    anchor: Vec<u8>,
    mirror: Mirror,
}

impl<'a> SyncSource<'a> {
    fn new(
        trusted_root: &Path,
        mirror: Mirror,
        publisher: &'a str,
        trust_key: &Path,
    ) -> Result<Self> {
        validate_publisher(publisher)?;
        let key = read_key(trust_key)?;
        let anchor = read_tuf_root(trusted_root)?;
        ensure!(
            mirror.document().len() as u64 <= ENDPOINTS_LIMIT,
            "mirror endpoints exceed the size limit"
        );
        Ok(Self {
            publisher,
            key,
            anchor,
            mirror,
        })
    }

    fn promote(&self, repo: &Path, release: &StagedRelease, binding: Option<&str>) -> Result<()> {
        let state = repo.join(".tuf");
        let same_catalog = read_current(repo).is_ok_and(|hash| hash == release.current);
        if !same_catalog {
            cache::record_retention(&state, release)?;
        }
        if let Some(binding) = binding {
            write_atomic(&state.join(PROFILE_BINDING), binding.as_bytes(), true)?;
        }
        write_atomic(
            &state.join("endpoints"),
            self.mirror.document().as_bytes(),
            false,
        )?;
        write_atomic(
            &repo.join("current"),
            format!("{}\n", release.current).as_bytes(),
            false,
        )?;
        if same_catalog {
            cache::record_retention(&state, release)?;
        }
        Ok(())
    }
}

enum SyncSelection<'a> {
    Current {
        binding: Option<&'a str>,
        required: &'a [&'a CatalogPin],
        guard: Option<&'a ImageGuard>,
        defer_checkpoint: bool,
    },
    Archive(&'a str),
}

fn initialize_cache(
    repo: &Path,
    source: &SyncSource<'_>,
    selection: &SyncSelection<'_>,
) -> Result<Root> {
    let SyncSource {
        publisher,
        key,
        anchor,
        ..
    } = source;
    let state = repo.join(".tuf");
    let origin = format!(
        "{CACHE_FORMAT}{}\n{publisher}\n{}\n",
        digest(anchor),
        hex::encode(key)
    );
    let published = path_present(&repo.join("current"))?;
    if !path_present(&state)? {
        ensure!(!published, "TUF sync requires a new repository cache");
        let next = tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in(repo)?;
        write_atomic(&next.path().join("origin"), origin.as_bytes(), true)?;
        if let SyncSelection::Archive(pin) = selection {
            write_atomic(
                &next.path().join(ARCHIVE),
                format!("{pin}\n").as_bytes(),
                true,
            )?;
        }
        let parent = File::open(repo)?;
        let name = next
            .path()
            .file_name()
            .context("missing cache staging name")?;
        renameat_with(&parent, name, &parent, ".tuf", RenameFlags::NOREPLACE)?;
        parent.sync_all()?;
    }
    let directory = open_state(repo)?;
    let trusted_exists = path_present(&state.join("trusted"))?;
    let ready = published || path_present(&state.join("endpoints"))?;
    let archive = path_present(&state.join(ARCHIVE))?;
    match selection {
        SyncSelection::Current { .. } => ensure!(
            !archive,
            "archive cache requires repo sync --archive-catalog"
        ),
        SyncSelection::Archive(pin) => {
            ensure!(
                archive && !published,
                "archive sync requires a dedicated cache"
            );
            ensure!(
                read_pointer(&state, ARCHIVE)? == *pin,
                "archive cache is bound to a different catalog"
            );
        }
    }
    if path_present(&state.join(PROFILE_BINDING))? {
        let SyncSelection::Current {
            binding: Some(binding),
            ..
        } = selection
        else {
            anyhow::bail!("TUF cache is managed by repo sync-profile");
        };
        ensure!(
            read_limited(directory.file(PROFILE_BINDING)?, 65)? == binding.as_bytes(),
            "TUF cache is bound to a different release profile or companion cache set"
        );
    }
    ensure!(
        read_limited(directory.file("origin")?, origin.len() as u64)? == origin.as_bytes(),
        "TUF cache was initialized with a different trust root or publisher"
    );
    ensure!(
        !ready || trusted_exists,
        "TUF cache is missing its trusted metadata"
    );
    ensure!(
        !published || path_present(&repo.join(PUBLISHER_CHECKPOINT))?,
        "TUF cache is missing its publisher catalog checkpoint"
    );
    Ok(directory)
}

// The caller holds the cache lock until promotion or failure.
fn stage_release(
    repo: &Path,
    source: &SyncSource<'_>,
    full: bool,
    selection: SyncSelection<'_>,
) -> Result<(StagedRelease, StagedMirror)> {
    let state = repo.join(".tuf");
    let directory = initialize_cache(repo, source, &selection)?;
    let scratch = TempDir::new_in(&state)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (release, verified) = runtime.block_on(async {
        let verified = load_mirror(
            &state,
            &directory,
            scratch.path(),
            source.mirror.clone(),
            Some(source.anchor.clone()),
        )
        .await?;
        prepare_objects(repo)?;
        let release = match selection {
            SyncSelection::Current {
                required,
                guard,
                defer_checkpoint,
                ..
            } => {
                materialize(
                    repo,
                    &verified,
                    source,
                    full,
                    required,
                    guard,
                    defer_checkpoint,
                )
                .await
            }
            SyncSelection::Archive(pin) => {
                materialize_archive(repo, &verified, source, full, pin).await
            }
        }?;
        Ok::<_, anyhow::Error>((release, verified))
    })?;
    Ok((
        release,
        StagedMirror {
            verified,
            _scratch: scratch,
            runtime,
        },
    ))
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
    if let Ok(verified) = &result {
        check_mirror_mode(state, verified)?;
    }
    // The library writes root, timestamp and snapshot only after verifying each role.
    // Retain that progress even when a later role or the release contents fail to download.
    if path_present(&scratch.join("root.json"))? {
        checkpoint(state, scratch, trusted_exists)?;
    }
    result.context("verifying TUF metadata")
}

fn check_mirror_mode(state: &Path, verified: &Repository) -> Result<()> {
    let targets = &verified.targets().signed;
    if path_present(&state.join(ARCHIVE))? {
        ensure!(
            targets
                .find_target(&TargetName::new("current")?, false)
                .is_err(),
            "archive mirror advertises a live catalog"
        );
        let pin = read_pointer(state, ARCHIVE)?;
        let marker = format!("{pin}\n");
        let size = marker.len() as u64;
        checked_target(
            verified,
            ARCHIVE,
            size,
            Some(&digest(marker.as_bytes())),
            Some(size),
        )
        .with_context(|| format!("checking the signed archive marker for catalog {pin}"))?;
    } else {
        ensure!(
            targets
                .find_target(&TargetName::new(ARCHIVE)?, false)
                .is_err(),
            "archive mirror requires repo sync --archive-catalog"
        );
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) struct DeltaBase<'a> {
    pub path: &'a str,
    pub hash: &'a str,
    pub length: u64,
}

pub(crate) fn fetch_payloads<'a>(
    publisher: &Publisher,
    root: &Root,
    blobs: impl IntoIterator<Item = (&'a str, u64, Option<DeltaBase<'a>>)>,
) -> Result<()> {
    fetch_payload_set(
        &publisher.repo,
        &publisher.name,
        &publisher.key,
        blobs,
        Some(root),
    )
}

pub(crate) fn fetch_repository_payloads(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    blobs: BTreeMap<&str, u64>,
) -> Result<()> {
    fetch_payload_set(
        repo,
        publisher,
        key,
        blobs.into_iter().map(|(hash, length)| (hash, length, None)),
        None,
    )
}

fn fetch_payload_set<'a>(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    blobs: impl IntoIterator<Item = (&'a str, u64, Option<DeltaBase<'a>>)>,
    root: Option<&Root>,
) -> Result<()> {
    let mut missing = Vec::new();
    for (hash, length, base) in blobs {
        validate_digest(hash)?;
        if !cached_object(repo, &format!("blobs/{hash}"), hash, length)? {
            missing.push((hash, length, base));
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
    check_cache_origin(&directory, publisher, key)?;
    let mirror = Mirror::read(&directory)?;
    let scratch = TempDir::new_in(&state)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let verified = load_mirror(&state, &directory, scratch.path(), mirror, None).await?;
        let mut full = Vec::new();
        for (hash, length, base) in missing {
            if let (Some(root), Some(base)) = (root, base)
                && try_delta(&verified, repo, root, base, hash, length)
                    .await
                    .is_ok()
            {
                continue;
            }
            full.push((hash, length));
        }
        fetch_verified_payloads(&verified, repo, full).await
    })
}

async fn try_delta(
    verified: &Repository,
    repo: &Path,
    image: &Root,
    base: DeltaBase<'_>,
    hash: &str,
    length: u64,
) -> Result<()> {
    ensure!(base.hash != hash, "delta base and result are identical");
    validate_digest(base.hash)?;
    let name = delta::target_name(base.hash, hash);
    let (target_name, target) =
        checked_target(verified, &name, delta::patch_limit(length), None, None)?;
    let full = checked_payload_target(verified, hash, length)?;
    ensure!(
        target.length < full.metadata.length,
        "delta is no smaller than the full payload"
    );
    let base_file = Root::open(repo)?
        .file(&format!("blobs/{}", base.hash))
        .and_then(|file| delta::verified_file(file, base.hash, base.length))
        .or_else(|_| delta::verified_file(image.file(base.path)?, base.hash, base.length))?;
    let parent = repo.join("blobs");
    let patch = download_target(verified, &name, &target_name, target, &parent).await?;
    let mut result = NamedTempFile::new_in(&parent)?;
    delta::reconstruct(
        &base_file,
        base.length,
        std::io::BufReader::new(patch.reopen()?),
        &mut result,
        length,
    )?;
    result.as_file_mut().rewind()?;
    ensure!(
        digest_reader(result.as_file_mut())? == hash,
        "reconstructed payload digest mismatch"
    );
    result.as_file().sync_all()?;
    result.persist(parent.join(hash))?;
    File::open(&parent)?.sync_all()?;
    Ok(())
}

async fn fetch_verified_payloads<'a>(
    verified: &Repository,
    repo: &Path,
    blobs: impl IntoIterator<Item = (&'a str, u64)>,
) -> Result<()> {
    stream::iter(blobs)
        .map(|(hash, length)| fetch_verified_payload(verified, repo, hash, length))
        .buffer_unordered(PAYLOAD_DOWNLOADS)
        .try_for_each(|()| async { Ok(()) })
        .await
}

struct PayloadTarget<'a> {
    name: String,
    tuf_name: TargetName,
    metadata: &'a tough::schema::Target,
    compressed: bool,
}

pub(crate) fn compressed_payload_name(hash: &str) -> String {
    format!("blobs-{hash}.zst")
}

fn checked_payload_target<'a>(
    verified: &'a Repository,
    hash: &str,
    length: u64,
) -> Result<PayloadTarget<'a>> {
    validate_digest(hash)?;
    let name = compressed_payload_name(hash);
    let tuf_name = TargetName::new(name.as_str())?;
    if let Ok(metadata) = verified.targets().signed.find_target(&tuf_name, false) {
        ensure!(
            metadata.length > 0 && metadata.length < length,
            "invalid signed length for {name}"
        );
        return Ok(PayloadTarget {
            name,
            tuf_name,
            metadata,
            compressed: true,
        });
    }

    let name = format!("blobs/{hash}");
    let (tuf_name, metadata) = checked_target(verified, &name, length, Some(hash), Some(length))?;
    Ok(PayloadTarget {
        name,
        tuf_name,
        metadata,
        compressed: false,
    })
}

async fn fetch_verified_payload(
    verified: &Repository,
    repo: &Path,
    hash: &str,
    length: u64,
) -> Result<()> {
    let target = checked_payload_target(verified, hash, length)?;
    if cached_object(repo, &format!("blobs/{hash}"), hash, length)? {
        return Ok(());
    }
    let parent = repo.join("blobs");
    let file = download_target(
        verified,
        &target.name,
        &target.tuf_name,
        target.metadata,
        &parent,
    )
    .await?;
    let result = if target.compressed {
        decompress_payload(file, &parent, hash, length)
            .with_context(|| format!("decoding {}", target.name))?
    } else {
        file
    };
    result.persist(parent.join(hash))?;
    File::open(&parent)?.sync_all()?;
    Ok(())
}

fn decompress_payload(
    mut compressed: NamedTempFile,
    parent: &Path,
    hash: &str,
    length: u64,
) -> Result<NamedTempFile> {
    compressed.as_file_mut().rewind()?;
    let mut result = NamedTempFile::new_in(parent)?;
    copy_compressed_payload(compressed.as_file_mut(), result.as_file_mut(), hash, length)?;
    result.as_file().sync_all()?;
    Ok(result)
}

pub(crate) fn copy_compressed_payload(
    source: impl Read,
    destination: &mut File,
    hash: &str,
    length: u64,
) -> Result<()> {
    let mut decoder = zstd::stream::read::Decoder::new(source)?;
    decoder.window_log_max(PAYLOAD_ZSTD_WINDOW_LOG)?;
    let copied = std::io::copy(&mut decoder.by_ref().take(length), destination)?;
    ensure!(
        copied == length && decoder.read(&mut [0])? == 0,
        "decompressed payload length mismatch for {hash}"
    );
    destination.rewind()?;
    ensure!(
        digest_reader(destination)? == hash,
        "decompressed payload digest mismatch for {hash}"
    );
    Ok(())
}

pub(super) fn fetch_source_archives(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    destination: &Root,
    missing: Vec<&FileDigest>,
) -> Result<()> {
    let state = repo.join(".tuf");
    ensure!(
        path_present(&state)?,
        "source archive object {} is missing and the repository has no TUF mirror",
        missing[0].sha256
    );
    let _lock = lock_repo(repo)?;
    let directory = open_state(repo)?;
    check_cache_origin(&directory, publisher, key)?;
    let mirror = Mirror::read(&directory)?;
    let scratch = TempDir::new_in(&state)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let verified = load_mirror(&state, &directory, scratch.path(), mirror, None).await?;
        let output = Root::fd_path(destination.as_file());
        let verified = &verified;
        let output = &output;
        stream::iter(missing)
            .map(|pin| async move {
                let name = format!("{}/{}", source_archive::OBJECTS, pin.sha256);
                let (target_name, target) =
                    checked_target(verified, &name, pin.size, Some(&pin.sha256), Some(pin.size))
                        .with_context(|| {
                            format!("checking archived source {} on the mirror", pin.sha256)
                        })?;
                let temp = download_target(verified, &name, &target_name, target, output).await?;
                publish_verified(destination, temp, pin)
            })
            .buffer_unordered(PAYLOAD_DOWNLOADS)
            .try_for_each(|()| async { Ok(()) })
            .await
    })
}

fn check_cache_origin(directory: &Root, publisher: &str, key: &[u8]) -> Result<()> {
    let identity = format!("\n{publisher}\n{}\n", hex::encode(key));
    let origin = String::from_utf8(read_limited(
        directory.file("origin")?,
        (CACHE_FORMAT.len() + 64 + identity.len()) as u64,
    )?)?;
    let anchor = origin
        .strip_prefix(CACHE_FORMAT)
        .context("unsupported TUF cache format")?
        .strip_suffix(&identity)
        .context("TUF cache is configured for a different publisher or key")?;
    validate_digest(anchor)
}

async fn materialize(
    repo: &Path,
    verified: &Repository,
    source: &SyncSource<'_>,
    full: bool,
    required: &[&CatalogPin],
    guard: Option<&ImageGuard>,
    defer_checkpoint: bool,
) -> Result<StagedRelease> {
    let publisher = source.publisher;
    let key = &source.key;
    let pointer = download(verified, "current", repo, 65, None, None)
        .await?
        .context("missing catalog pointer download")?;
    let hash = parse_current(&String::from_utf8(read_limited(pointer.reopen()?, 65)?)?)?;
    fetch_object(verified, repo, "catalogs", &hash, CATALOG_LIMIT, None).await?;
    let catalog = read_catalog(repo, &hash, publisher, key)?;
    if let Some(guard) = guard {
        guard.check(repo, source, &hash, &catalog)?;
    }
    let checkpoint = repo.join(PUBLISHER_CHECKPOINT);
    let trusted = path_present(&checkpoint)?
        .then(|| read_pointer(repo, PUBLISHER_CHECKPOINT))
        .transpose()?;
    if trusted.as_deref() != Some(&hash) {
        if let Some(previous) = trusted {
            read_catalog(repo, &previous, publisher, key)?.check_successor(&catalog, &previous)?;
        }
        // Remember the signed catalog even when its payloads cannot be fetched.
        if !defer_checkpoint {
            record_catalog_checkpoint(repo, &hash)?;
        }
    }
    for pin in required.iter().filter(|pin| pin.sha256 == hash) {
        pin.check_lineage(&catalog, key)?;
    }
    cache_file_index(verified, repo, &catalog).await?;
    let mut entries = BTreeMap::new();
    let mut attestations = BTreeSet::new();
    let mut rebuild_objects = BTreeMap::new();
    let mut retained_hashes = Vec::new();
    if verified
        .targets()
        .signed
        .find_target(&TargetName::new(RETAINED)?, false)
        .is_ok()
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
            for required in required.iter().filter(|required| required.sha256 == pin) {
                required.check_lineage(&previous, key)?;
            }
            cache_file_index(verified, repo, &previous).await?;
            collect_rebuild_objects(&previous, &mut rebuild_objects, &mut attestations);
            entries.extend(previous.entries);
            attestations.extend(previous.attestations.into_values());
            retained_hashes.push(pin.to_owned());
        }
    }
    for pin in required {
        ensure!(
            pin.sha256 == hash || retained_hashes.contains(&pin.sha256),
            "mirror for {publisher} does not advertise required catalog {} as current or retained",
            pin.sha256
        );
    }
    collect_rebuild_objects(&catalog, &mut rebuild_objects, &mut attestations);
    cache_rebuild_objects(verified, repo, &catalog, rebuild_objects).await?;
    entries.extend(catalog.entries);
    attestations.extend(catalog.attestations.into_values());
    cache_objects(verified, repo, publisher, entries, attestations, full).await?;
    Ok(StagedRelease {
        current: hash,
        retained: retained_hashes,
    })
}

async fn materialize_archive(
    repo: &Path,
    verified: &Repository,
    source: &SyncSource<'_>,
    full: bool,
    hash: &str,
) -> Result<StagedRelease> {
    fetch_object(verified, repo, "catalogs", hash, CATALOG_LIMIT, None).await?;
    let catalog = read_catalog(repo, hash, source.publisher, &source.key)?;
    cache_file_index(verified, repo, &catalog).await?;
    let mut rebuild_objects = BTreeMap::new();
    let mut attestations = BTreeSet::new();
    collect_rebuild_objects(&catalog, &mut rebuild_objects, &mut attestations);
    cache_rebuild_objects(verified, repo, &catalog, rebuild_objects).await?;
    attestations.extend(catalog.attestations.into_values());
    cache_objects(
        verified,
        repo,
        source.publisher,
        catalog.entries,
        attestations,
        full,
    )
    .await?;
    Ok(StagedRelease {
        current: hash.to_owned(),
        retained: Vec::new(),
    })
}

async fn cache_file_index(verified: &Repository, repo: &Path, catalog: &Catalog) -> Result<()> {
    if let Some(hash) = &catalog.file_index {
        fetch_object(
            verified,
            repo,
            FILE_INDEX_OBJECTS,
            hash,
            FILE_INDEX_LIMIT,
            None,
        )
        .await?;
    }
    Ok(())
}

async fn cache_rebuild_objects(
    verified: &Repository,
    repo: &Path,
    catalog: &Catalog,
    rebuild_objects: BTreeMap<String, u64>,
) -> Result<()> {
    stream::iter(rebuild_objects)
        .map(|(hash, limit)| async move {
            fetch_object(verified, repo, rebuild::OBJECTS, &hash, limit, None).await
        })
        .buffer_unordered(METADATA_DOWNLOADS)
        .try_for_each(|()| async { Ok(()) })
        .await?;
    if catalog.source_archives_required {
        let mut recipes = BTreeSet::new();
        for inputs in catalog.build_inputs.values() {
            if recipes.insert(&inputs.recipe) {
                for pin in source_archive::archived_pins(repo, inputs)? {
                    let name = format!("{}/{}", source_archive::OBJECTS, pin.sha256);
                    checked_target(verified, &name, pin.size, Some(&pin.sha256), Some(pin.size))
                        .with_context(|| {
                            format!(
                                "checking required archived source {} on the mirror",
                                pin.sha256
                            )
                        })?;
                }
            }
        }
    }
    Ok(())
}

fn collect_rebuild_objects(
    catalog: &Catalog,
    objects: &mut BTreeMap<String, u64>,
    attestations: &mut BTreeSet<String>,
) {
    for (id, inputs) in &catalog.build_inputs {
        for (hash, limit) in inputs.objects() {
            objects
                .entry(hash.to_owned())
                .and_modify(|current| *current = (*current).min(limit))
                .or_insert(limit);
        }
        if let Some((_, Some(hash))) = catalog.package_record(id) {
            // Include admitted evidence for withdrawn rebuilds in a fresh cache.
            attestations.insert(hash.to_owned());
        }
    }
}

fn record_catalog_checkpoint(repo: &Path, hash: &str) -> Result<()> {
    write_atomic(
        &repo.join(PUBLISHER_CHECKPOINT),
        format!("{hash}\n").as_bytes(),
        false,
    )
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
    entries: BTreeMap<String, CatalogEntry>,
    attestations: BTreeSet<String>,
    full: bool,
) -> Result<()> {
    let packages: Vec<_> = stream::iter(entries)
        .map(|(id, entry)| async move {
            fetch_object(
                verified,
                repo,
                "manifests",
                entry.manifest_digest(),
                MANIFEST_LIMIT,
                None,
            )
            .await?;
            read_package(repo, publisher, &id, &entry)
        })
        .buffer_unordered(METADATA_DOWNLOADS)
        .try_collect()
        .await?;
    for (sha256, length) in release_blobs(&packages)? {
        if full {
            fetch_verified_payload(verified, repo, sha256, length).await?;
        } else {
            checked_payload_target(verified, sha256, length)?;
        }
    }
    stream::iter(attestations)
        .map(|sha256| async move {
            fetch_object(
                verified,
                repo,
                "attestations",
                &sha256,
                ATTESTATION_LIMIT,
                None,
            )
            .await
        })
        .buffer_unordered(METADATA_DOWNLOADS)
        .try_for_each(|()| async { Ok(()) })
        .await
}

fn release_blobs<'a>(
    packages: impl IntoIterator<Item = &'a Published>,
) -> Result<BTreeMap<&'a str, u64>> {
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

pub(crate) fn cached_object(repo: &Path, name: &str, hash: &str, length: u64) -> Result<bool> {
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
    Ok(Some(
        download_target(verified, name, &target_name, target, &repo.join(parent)).await?,
    ))
}

async fn download_target(
    verified: &Repository,
    name: &str,
    target_name: &TargetName,
    target: &tough::schema::Target,
    parent: &Path,
) -> Result<NamedTempFile> {
    let mut file = NamedTempFile::new_in(parent)?;
    let stream = verified
        .read_target(target_name)
        .await?
        .with_context(|| format!("missing TUF target {name}"))?;
    pin_mut!(stream);
    let mut written = 0u64;
    while let Some(bytes) = stream
        .try_next()
        .await
        .with_context(|| format!("reading target {name}"))?
    {
        written = written
            .checked_add(bytes.len() as u64)
            .filter(|length| *length <= target.length)
            .with_context(|| format!("download exceeds signed length for {name}"))?;
        file.write_all(&bytes)?;
    }
    ensure!(written == target.length, "short download for {name}");
    file.as_file().sync_all()?;
    Ok(file)
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
    export_targets(
        repo,
        publisher,
        trust_key,
        output,
        ExportSelection::Current(retained),
        false,
        &[],
    )
}

/// Also include large source targets attached to the signed catalog.
pub fn export_with_sources(
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    output: &Path,
    retained: &[String],
) -> Result<String> {
    export_targets(
        repo,
        publisher,
        trust_key,
        output,
        ExportSelection::Current(retained),
        true,
        &[],
    )
}

/// Include optional payload patches based on earlier signed catalogs.
pub fn export_with_deltas(
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    output: &Path,
    retained: &[String],
    include_sources: bool,
    delta_from: &[String],
) -> Result<String> {
    export_targets(
        repo,
        publisher,
        trust_key,
        output,
        ExportSelection::Current(retained),
        include_sources,
        delta_from,
    )
}

/// Export a pinned catalog to a separate archive target set without a live pointer.
pub fn export_archive(
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    output: &Path,
    pin: &str,
    include_sources: bool,
) -> Result<String> {
    export_targets(
        repo,
        publisher,
        trust_key,
        output,
        ExportSelection::Archive(pin),
        include_sources,
        &[],
    )
}

enum ExportSelection<'a> {
    Current(&'a [String]),
    Archive(&'a str),
}

fn export_targets(
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    output: &Path,
    selection: ExportSelection<'_>,
    include_sources: bool,
    delta_from: &[String],
) -> Result<String> {
    let key_der = read_key_der(trust_key)?;
    let key = parse_public_key(&key_der)?;
    let current = read_current(repo)?;
    let catalog = read_catalog(repo, &current, publisher, key)?;
    let (pointer, hash, catalog, retained) = match selection {
        ExportSelection::Current(retained) => {
            let pins = retained_catalogs(retained.iter().map(String::as_str), &current)?;
            ("current", current, catalog, pins)
        }
        ExportSelection::Archive(pin) => {
            let archived = if pin == current {
                catalog
            } else {
                let archived = read_catalog(repo, pin, publisher, key)?;
                archived.check_successor(&catalog, pin).with_context(|| {
                    format!("checking archive catalog {pin} against current {current}")
                })?;
                archived
            };
            (ARCHIVE, pin.to_owned(), archived, BTreeSet::new())
        }
    };
    let root = Root::open(repo)?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stage = TempDir::new_in(parent)?;
    let mut blobs = BTreeMap::new();
    for pin in &retained {
        let previous = read_catalog(repo, pin, publisher, key)?;
        previous
            .check_successor(&catalog, pin)
            .with_context(|| format!("checking retained catalog {pin} against current {hash}"))?;
        let release = catalog_snapshot(repo, previous, (*pin).to_owned(), key)?;
        export_catalog(&root, stage.path(), &release, &mut blobs)?;
    }
    let include_sources = include_sources || catalog.source_archives_required;
    let release = catalog_snapshot(repo, catalog, hash.clone(), key)?;
    export_catalog(&root, stage.path(), &release, &mut blobs)?;
    for (hash, length) in blobs {
        export_payload(&root, stage.path(), &hash, length)?;
    }
    if !delta_from.is_empty() {
        let pins = retained_catalogs(delta_from.iter().map(String::as_str), &hash)
            .context("checking delta base catalog pins")?;
        let current = read_catalog(repo, &hash, publisher, key)?;
        let mut seen = BTreeSet::new();
        for pin in pins {
            let previous = read_catalog(repo, pin, publisher, key)?;
            previous
                .check_successor(&current, pin)
                .with_context(|| format!("checking delta base catalog {pin}"))?;
            let previous = catalog_snapshot(repo, previous, pin.to_owned(), key)?;
            delta::export(&root, stage.path(), &previous, &release, &mut seen)?;
        }
    }
    if include_sources {
        for inputs in release
            .build_inputs
            .values()
            .filter(|inputs| inputs.sources_archived)
        {
            for pin in source_archive::archived_pins(repo, inputs)? {
                export_object(
                    &root,
                    stage.path(),
                    source_archive::OBJECTS,
                    &pin.sha256,
                    pin.size,
                    Some(pin.size),
                )?;
            }
        }
    }
    if !retained.is_empty() {
        let mut document = String::new();
        for pin in retained {
            document.push_str(pin);
            document.push('\n');
        }
        write_atomic(&stage.path().join(RETAINED), document.as_bytes(), false)?;
    }
    if pointer == "current" {
        write_atomic(
            &stage.path().join(PUBLISHER_NAME_TARGET),
            publisher.as_bytes(),
            false,
        )?;
        write_atomic(&stage.path().join(PUBLISHER_KEY_TARGET), &key_der, false)?;
    }
    write_atomic(
        &stage.path().join(pointer),
        format!("{hash}\n").as_bytes(),
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
    Ok(hash)
}

fn export_catalog(
    root: &Root,
    output: &Path,
    release: &Snapshot,
    blobs: &mut BTreeMap<String, u64>,
) -> Result<()> {
    export_object(
        root,
        output,
        "catalogs",
        &release.sha256,
        CATALOG_LIMIT,
        None,
    )?;
    if let Some(hash) = &release.file_index {
        export_object(
            root,
            output,
            FILE_INDEX_OBJECTS,
            hash,
            FILE_INDEX_LIMIT,
            None,
        )?;
    }
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
        let saved = blobs.entry(hash.to_owned()).or_insert(length);
        ensure!(*saved == length, "conflicting payload lengths for {hash}");
    }
    for hash in release.builds.attestations.values() {
        export_object(root, output, "attestations", hash, ATTESTATION_LIMIT, None)?;
    }
    for inputs in release.build_inputs.values() {
        for (hash, limit) in inputs.objects() {
            export_object(root, output, rebuild::OBJECTS, hash, limit, None)?;
        }
    }
    Ok(())
}

fn export_payload(root: &Root, output: &Path, hash: &str, length: u64) -> Result<()> {
    export_object(root, output, "blobs", hash, length, Some(length))?;
    let raw_path = output.join(format!("blobs-{hash}"));
    let compressed_path = output.join(compressed_payload_name(hash));
    let compressed = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&compressed_path)?;
    let mut encoder = zstd::stream::write::Encoder::new(compressed, PAYLOAD_ZSTD_LEVEL)?;
    encoder.window_log(PAYLOAD_ZSTD_WINDOW_LOG)?;
    encoder.set_pledged_src_size(Some(length))?;
    std::io::copy(&mut File::open(&raw_path)?, &mut encoder)?;
    let compressed = encoder.finish()?;
    if compressed.metadata()?.len() < length {
        compressed.sync_all()?;
        std::fs::remove_file(raw_path)?;
    } else {
        std::fs::remove_file(compressed_path)?;
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
