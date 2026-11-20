//! Publish the current catalog as a signed TUF tree in an HTTPS web root,
//! and keep its timestamp fresh.
//!
//! The publishing host keeps a private record, `.tuf-publication.json` in the
//! publishing repository, of the metadata version it reserved last and of
//! what it published. The web root is not trusted: every version comes from
//! the record, and a refresh signs only over a snapshot whose digest matches
//! it. Each deployment is verified from the original TUF root as a new
//! client would before anything is exposed. Files are added beside the
//! served ones, and `timestamp.json` is replaced last, so clients only ever
//! see a complete release.

use std::{
    fs::{self, File},
    io::Write,
    num::NonZeroU64,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rustix::{
    fs::{FlockOperation, flock},
    process::geteuid,
};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use super::{
    DESCRIPTOR, DESCRIPTOR_LIMIT, Mirror, MirrorSources, ROLE_LIMIT, ROOT_LIMIT, RefreshMetadata,
    ReleaseKeys, SignExport, SyncOptions, export, export_mirror_list, refresh_metadata,
    sign_export, sync_mirrors,
    web_files::{copy_public, create_public_dir, fd_url, same_contents},
};
use crate::{
    model::digest,
    repo::{
        keys::trust::Pinned, lock_repo, pinned_publisher, read_current, read_current_catalog,
        read_limited,
    },
};

const STATE: &str = ".tuf-publication.json";
const STATE_LIMIT: u64 = 64 * 1024;
const MAX_VERSION: u64 = 999_999_999_999_999;
/// Re-sign a snapshot that would expire within a day of the new timestamp.
const SNAPSHOT_MARGIN: Duration = Duration::from_secs(24 * 60 * 60);
/// Warn when the targets metadata expires within this time.
const TARGETS_WARNING: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// How long newly signed metadata stays valid.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Lifetimes {
    pub targets: Duration,
    pub snapshot: Duration,
    pub timestamp: Duration,
}

impl Default for Lifetimes {
    fn default() -> Self {
        Self {
            targets: days(30),
            snapshot: days(30),
            timestamp: days(7),
        }
    }
}

const fn days(count: u64) -> Duration {
    Duration::from_secs(count * 24 * 60 * 60)
}

/// Parse a lifetime such as `7d`, `12h`, `30 days`, or `1 hours`.
pub(crate) fn parse_lifetime(text: &str) -> Result<Duration> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let (number, unit) = text.split_at(digits);
    let count = number
        .parse::<u64>()
        .ok()
        .filter(|count| (1..=9999).contains(count) && !number.starts_with('0'));
    let hours = match unit.trim_start() {
        "d" | "day" | "days" => 24,
        "h" | "hour" | "hours" => 1,
        _ => 0,
    };
    match count {
        Some(count) if hours > 0 => Ok(Duration::from_secs(count * hours * 3600)),
        _ => {
            bail!("expected a lifetime of 1 to 9999 days or hours, such as 7d or 12h, not {text:?}")
        }
    }
}

/// One web root served from one publishing repository.
pub(crate) struct Publication<'a> {
    /// The private publishing repository.
    pub repo: &'a Path,
    /// The HTTPS document directory.
    pub output: &'a Path,
    /// The original signed TUF root that clients pin.
    pub tuf_root: &'a Path,
    /// The TUF root to sign with, when it has been rotated since.
    pub active_tuf_root: Option<&'a Path>,
    /// The publisher trust root that clients pin.
    pub trust_root: &'a Path,
    /// A directory of TUF role keys: `ROLE.pem`, `ROLE.der`,
    /// `ROLE.signer.kdl`, or the same names as `ROLE-NAME.*`.
    pub keys: &'a Path,
    /// The publisher's list of public mirrors to sign for clients.
    pub mirror_list: Option<&'a Path>,
    pub lifetimes: Lifetimes,
    pub objects_threshold: NonZeroU64,
    /// The highest metadata version already published, for a web root
    /// whose publication record was lost. Take it from trusted records.
    pub initial_version: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    format: u32,
    mirror: PathBuf,
    version: u64,
    published: Option<Published>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Published {
    catalog: String,
    expires: String,
    sha256: String,
    snapshot: SnapshotRecord,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SnapshotRecord {
    version: u64,
    sha256: String,
    expires: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct Descriptor {
    schema_version: u32,
    kind: String,
    publisher: String,
    tuf_root: String,
    trust_root: String,
}

/// An open, locked web root and the staging area beside it.
struct Session {
    repo: PathBuf,
    mirror: PathBuf,
    stage: TempDir,
    publisher: String,
    catalog: String,
    previous: u64,
    published: Option<Published>,
    root_version: u64,
    latest_root_version: u64,
    _repo_lock: File,
    _mirror_lock: File,
}

impl Session {
    fn open(publication: &Publication<'_>, catalog: Option<&str>, refresh: bool) -> Result<Self> {
        let repo = fs::canonicalize(publication.repo)
            .with_context(|| format!("opening {}", publication.repo.display()))?;
        let mirror = fs::canonicalize(publication.output)
            .with_context(|| format!("opening the web root {}", publication.output.display()))?;
        let keys = fs::canonicalize(publication.keys)
            .with_context(|| format!("opening the key directory {}", publication.keys.display()))?;
        ensure!(
            mirror.is_dir()
                && mirror != Path::new("/")
                && !repo.starts_with(&mirror)
                && !mirror.starts_with(&repo),
            "use a dedicated web root separate from the publishing repository"
        );
        ensure!(
            !keys.starts_with(&mirror),
            "keep signing keys outside the web root"
        );
        let publisher = pinned_publisher(publication.trust_root)?;
        ensure!(
            repo.join("publish.lock").is_file(),
            "expected a CPS publishing repository at {}",
            repo.display()
        );
        // Serialize updates to this web root and hold the catalog stable until publication.
        let repo_lock = lock_repo(&repo)?;
        // A refresh re-signs what is deployed, whatever the workspace has published since.
        let current = if refresh {
            None
        } else {
            Some(read_current(&repo).context("publish a package before deploying")?)
        };
        let mirror_lock = File::open(&mirror)?;
        flock(&mirror_lock, FlockOperation::LockExclusive)?;
        for directory in ["metadata", "targets"] {
            let path = mirror.join(directory);
            match path.symlink_metadata() {
                Ok(metadata) => ensure!(
                    metadata.is_dir(),
                    "invalid web root directory {}",
                    path.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let timestamp = mirror.join("metadata/timestamp.json");
        ensure!(
            timestamp
                .symlink_metadata()
                .map_or(true, |metadata| metadata.is_file()),
            "invalid web root timestamp path"
        );

        // Metadata versions are reserved on the publishing host before the web root exposes them.
        let (previous, published) = match read_state(&repo, &mirror)? {
            Some(state) => (state.version, state.published),
            None if refresh => {
                bail!("a refresh needs the publication record of a completed deployment")
            }
            None => match publication.initial_version {
                Some(0) => bail!("use the highest previously published metadata version"),
                Some(version) => (version, None),
                None => {
                    ensure!(
                        !mirror.join(DESCRIPTOR).exists()
                            && !timestamp.exists()
                            && !has_versioned_metadata(&mirror.join("metadata"))?,
                        "the publication record of an existing web root is missing; restore it, \
                         or pass the highest published metadata version from trusted records"
                    );
                    (0, None)
                }
            },
        };
        ensure!(
            previous < MAX_VERSION,
            "invalid or exhausted TUF metadata version in the publication record"
        );
        let catalog = match (current, &published) {
            (Some(current), _) => {
                ensure!(
                    catalog.is_none_or(|catalog| catalog == current),
                    "the approved catalog is no longer current"
                );
                current
            }
            (None, Some(published)) => {
                ensure!(
                    catalog.is_none_or(|catalog| catalog == published.catalog),
                    "the web root serves catalog {}, not the approved one",
                    published.catalog
                );
                published.catalog.clone()
            }
            (None, None) => bail!(
                "no completed deployment to refresh; deploy the current catalog with cps repo deploy"
            ),
        };

        let parent = mirror.parent().context("missing web root parent")?;
        let stage = tempfile::Builder::new()
            .prefix(".cps-mirror.")
            .tempdir_in(parent)?;
        let bootstrap = stage.path().join("bootstrap-root.json");
        fs::copy(publication.tuf_root, &bootstrap)?;
        fs::copy(publication.trust_root, stage.path().join("trust-root.json"))?;
        let bootstrap_version = root_version(&bootstrap)?;

        // Every published root stays published, so clients can walk the history.
        let roots = stage.path().join("roots");
        fs::create_dir(&roots)?;
        stage_root(&roots, &bootstrap, bootstrap_version)?;
        let mut latest_root_version = bootstrap_version;
        if let Ok(entries) = fs::read_dir(mirror.join("metadata")) {
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                let Some(number) = name
                    .to_str()
                    .and_then(|name| name.strip_suffix(".root.json"))
                else {
                    continue;
                };
                let version = parse_version(number).with_context(|| {
                    format!("invalid versioned TUF root {}", entry.path().display())
                })?;
                let metadata = entry.path().symlink_metadata()?;
                ensure!(
                    metadata.is_file() && metadata.len() <= ROOT_LIMIT,
                    "invalid versioned TUF root {}",
                    entry.path().display()
                );
                stage_root(&roots, &entry.path(), version)?;
                latest_root_version = latest_root_version.max(version);
            }
        }
        // Without an explicit root, keep signing with the newest published one.
        // Verification below follows the published history from the original
        // root, so a root that history does not lead to is refused.
        let active = stage.path().join("root.json");
        match publication.active_tuf_root {
            Some(path) => fs::copy(path, &active)?,
            None => fs::copy(
                roots.join(format!("{latest_root_version}.root.json")),
                &active,
            )?,
        };
        let root_version = root_version(&active)?;
        let expires: jiff::Timestamp = json(&active, ROOT_LIMIT)?["signed"]["expires"]
            .as_str()
            .context("missing TUF root expiry")?
            .parse()?;
        ensure!(expires > jiff::Timestamp::now(), "the TUF root has expired");
        ensure!(
            root_version == latest_root_version || root_version == latest_root_version + 1,
            "use the latest published TUF root or its immediate successor"
        );
        stage_root(&roots, &active, root_version)?;
        Ok(Self {
            repo,
            mirror,
            stage,
            publisher,
            catalog,
            previous,
            published,
            root_version,
            latest_root_version,
            _repo_lock: repo_lock,
            _mirror_lock: mirror_lock,
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.stage.path().join(name)
    }

    fn version(&self) -> u64 {
        self.previous + 1
    }

    fn record(&self, published: Option<&Published>) -> Result<()> {
        let state = State {
            format: 1,
            mirror: self.mirror.clone(),
            version: self.version(),
            published: published.cloned(),
        };
        let mut file = tempfile::Builder::new()
            .prefix(".tuf-publication.")
            .permissions(fs::Permissions::from_mode(0o600))
            .tempfile_in(&self.repo)?;
        file.write_all(&serde_json::to_vec(&state)?)?;
        file.as_file().sync_all()?;
        file.persist(self.repo.join(STATE))?;
        File::open(&self.repo)?.sync_all()?;
        Ok(())
    }

    /// Verify staged metadata from the original root, as a new client would.
    fn verify(&self, metadata: &Path, targets: &Path) -> Result<()> {
        let metadata = File::open(metadata)?;
        let targets = File::open(targets)?;
        let url = |directory: &File| fd_url(directory).map(String::from);
        let cache = self.path("verified");
        sync_mirrors(
            &cache,
            &self.path("bootstrap-root.json"),
            MirrorSources::explicit(Mirror::new(&url(&metadata)?, &url(&targets)?)?),
            &self.publisher,
            &self.path("trust-root.json"),
            SyncOptions {
                index_only: true,
                image: None,
            },
            std::slice::from_ref(&self.catalog),
        )
        .context("verifying the signed metadata as a new client")?;
        ensure!(
            root_version(&cache.join(".tuf/trusted/root.json"))? == self.root_version,
            "verification did not reach the active TUF root through the published root history"
        );
        Ok(())
    }

    /// Expose an immutable file beside the served ones, or check that it is unchanged.
    fn publish_immutable(&self, source: &Path, destination: &Path) -> Result<()> {
        match destination.symlink_metadata() {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file() && same_contents(source, destination)?,
                    "conflicting web root object {}",
                    destination.display()
                );
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A hard link exposes a complete copy that outlives the stage.
                let next = self.path("next");
                copy_public(source, &next)?;
                fs::hard_link(&next, destination)?;
                fs::remove_file(next)?;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Replace a mutable file such as `timestamp.json` in one rename.
    fn replace(&self, source: &Path, destination: &Path) -> Result<()> {
        let next = self.path("replace.next");
        copy_public(source, &next)?;
        File::open(&next)?.sync_all()?;
        fs::rename(&next, destination)?;
        File::open(destination.parent().context("missing parent")?)?.sync_all()?;
        Ok(())
    }
}

fn read_state(repo: &Path, mirror: &Path) -> Result<Option<State>> {
    let path = repo.join(STATE);
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file()
            && metadata.uid() == geteuid().as_raw()
            && metadata.mode() & 0o777 == 0o600,
        "the publication record must be a private file owned by the signer"
    );
    let state: State = serde_json::from_slice(&read_limited(File::open(&path)?, STATE_LIMIT)?)
        .context("invalid publication record")?;
    ensure!(
        state.format == 1,
        "invalid publication record: unsupported format {}",
        state.format
    );
    ensure!(
        state.mirror == mirror,
        "the publication record is for a different web root: {}",
        state.mirror.display()
    );
    Ok(Some(state))
}

fn has_versioned_metadata(metadata: &Path) -> Result<bool> {
    let Ok(entries) = fs::read_dir(metadata) else {
        return Ok(false);
    };
    for entry in entries {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".root.json")
            || name.ends_with(".snapshot.json")
            || name.ends_with(".targets.json")
            || name.contains(".objects-")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn parse_version(text: &str) -> Result<u64> {
    let version = text
        .parse::<u64>()
        .ok()
        .filter(|version| (1..=MAX_VERSION).contains(version) && !text.starts_with('0'))
        .with_context(|| format!("invalid metadata version {text:?}"))?;
    Ok(version)
}

fn json(path: &Path, limit: u64) -> Result<serde_json::Value> {
    serde_json::from_slice(&read_limited(File::open(path)?, limit)?)
        .with_context(|| format!("parsing {}", path.display()))
}

fn root_version(path: &Path) -> Result<u64> {
    let root = json(path, ROOT_LIMIT)?;
    ensure!(
        root["signed"]["consistent_snapshot"] == true,
        "use a signed TUF root with consistent snapshots enabled"
    );
    root["signed"]["version"]
        .as_u64()
        .filter(|version| (1..=MAX_VERSION).contains(version))
        .context("invalid TUF root version")
}

fn stage_root(roots: &Path, source: &Path, version: u64) -> Result<()> {
    let destination = roots.join(format!("{version}.root.json"));
    if destination.exists() {
        ensure!(
            fs::read(source)? == fs::read(&destination)?,
            "conflicting TUF root version {version}"
        );
    } else {
        fs::copy(source, destination)?;
    }
    Ok(())
}

fn sha256(path: &Path) -> Result<String> {
    Ok(digest(&fs::read(path)?))
}

fn expiry_after(lifetime: Duration) -> Result<String> {
    let time = jiff::Timestamp::now().checked_add(jiff::SignedDuration::try_from(lifetime)?)?;
    Ok(time.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
}

fn timestamp_of(text: &str) -> Result<jiff::Timestamp> {
    text.parse()
        .with_context(|| format!("invalid metadata expiry {text:?}"))
}

/// Key sources for one role, in a stable order.
fn role_keys(keys: &Path, role: &str, mirror: &Path) -> Result<Vec<PathBuf>> {
    let mut exact = Vec::new();
    let mut named = Vec::new();
    for entry in fs::read_dir(keys)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        for extension in ["pem", "der", "signer.kdl"] {
            let Some(stem) = name
                .strip_suffix(extension)
                .and_then(|stem| stem.strip_suffix('.'))
            else {
                continue;
            };
            if stem == role {
                exact.push(name.to_owned());
            } else if stem
                .strip_prefix(role)
                .is_some_and(|rest| rest.starts_with('-') && rest.len() > 1)
            {
                named.push(name.to_owned());
            }
        }
    }
    exact.sort();
    named.sort();
    exact
        .into_iter()
        .chain(named)
        .map(|name| {
            let path = fs::canonicalize(keys.join(&name))?;
            ensure!(
                path.is_file() && !path.starts_with(mirror),
                "invalid {role} key source {}",
                path.display()
            );
            Ok(if name.ends_with(".signer.kdl") {
                PathBuf::from(format!("command:{}", path.display()))
            } else {
                path
            })
        })
        .collect()
}

/// Sign and publish the current catalog. Returns a summary line.
pub(crate) fn deploy_mirror(
    publication: &Publication<'_>,
    catalog: Option<&str>,
) -> Result<String> {
    let session = Session::open(publication, catalog, false)?;
    let version = session.version();
    let mirror = &session.mirror;
    let keys = fs::canonicalize(publication.keys)?;
    let mut signing = Vec::new();
    for role in ["targets", "snapshot", "timestamp"] {
        let sources = role_keys(&keys, role, mirror)?;
        ensure!(
            !sources.is_empty(),
            "missing {role} key source in {}",
            keys.display()
        );
        signing.push(sources);
    }
    let objects = role_keys(&keys, "objects", mirror)?;

    let export_dir = session.path("export");
    let exported = export(
        &session.repo,
        &session.publisher,
        &session.path("trust-root.json"),
        &export_dir,
    )?;
    ensure!(exported == session.catalog, "exported a different catalog");
    if let Some(list) = publication.mirror_list {
        export_mirror_list(&export_dir, list)?;
    }

    let trust_root = fs::read(session.path("trust-root.json"))?;
    let descriptor = Descriptor {
        schema_version: 1,
        kind: "publisher-bootstrap".to_owned(),
        publisher: session.publisher.clone(),
        tuf_root: STANDARD.encode(fs::read(session.path("bootstrap-root.json"))?),
        trust_root: STANDARD.encode(&trust_root),
    };
    let mut descriptor_bytes = serde_json::to_vec_pretty(&descriptor)?;
    descriptor_bytes.push(b'\n');
    fs::write(session.path(DESCRIPTOR), &descriptor_bytes)?;
    let replace_descriptor =
        check_descriptor(&session, &mirror.join(DESCRIPTOR), &descriptor, &trust_root)?;

    let release_expires = expiry_after(publication.lifetimes.targets)?;
    let timestamp_expires = expiry_after(publication.lifetimes.timestamp)?;
    let signed = session.path("signed");
    sign_export(SignExport {
        root: &session.path("root.json"),
        export: &export_dir,
        output: &signed,
        keys: ReleaseKeys {
            targets: &signing[0],
            snapshot: &signing[1],
            timestamp: &signing[2],
            objects: &objects,
            objects_threshold: publication.objects_threshold,
        },
        version: NonZeroU64::new(version).context("invalid metadata version")?,
        release_expires: &release_expires,
        timestamp_expires: &timestamp_expires,
    })?;
    for entry in fs::read_dir(session.path("roots"))? {
        let entry = entry?;
        fs::copy(
            entry.path(),
            signed.join("metadata").join(entry.file_name()),
        )?;
    }
    session.verify(&signed.join("metadata"), &signed.join("targets"))?;

    let metadata = signed.join("metadata");
    let snapshot = metadata.join(format!("{version}.snapshot.json"));
    let published = Published {
        catalog: session.catalog.clone(),
        expires: timestamp_expires,
        sha256: sha256(&metadata.join("timestamp.json"))?,
        snapshot: SnapshotRecord {
            version,
            sha256: sha256(&snapshot)?,
            expires: release_expires,
        },
    };
    // Reserve the version before the web root can expose it.
    session.record(None)?;

    let served_metadata = mirror.join("metadata");
    let served_targets = mirror.join("targets");
    for directory in [&served_metadata, &served_targets] {
        if !directory.exists() {
            create_public_dir(directory)?;
        }
    }
    let device = session.stage.path().metadata()?.dev();
    for directory in [mirror, &served_metadata, &served_targets] {
        ensure!(
            directory.metadata()?.dev() == device,
            "the web root and its parent must be on the same filesystem"
        );
    }
    for entry in fs::read_dir(signed.join("targets"))? {
        let entry = entry?;
        session.publish_immutable(&entry.path(), &served_targets.join(entry.file_name()))?;
    }
    for entry in fs::read_dir(&metadata)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&format!("{version}.objects-")) {
            session.publish_immutable(&entry.path(), &served_metadata.join(&*name))?;
        }
    }
    for name in [
        format!("{version}.targets.json"),
        format!("{version}.snapshot.json"),
    ] {
        session.publish_immutable(&metadata.join(&name), &served_metadata.join(&name))?;
    }
    let active = format!("{}.root.json", session.root_version);
    for entry in fs::read_dir(session.path("roots"))? {
        let entry = entry?;
        if entry.file_name() != active.as_str() {
            session.publish_immutable(&entry.path(), &served_metadata.join(entry.file_name()))?;
        }
    }
    if !replace_descriptor {
        session.publish_immutable(&session.path(DESCRIPTOR), &mirror.join(DESCRIPTOR))?;
    }
    File::open(session.stage.path())?.sync_all()?;
    // A new root may revoke the old timestamp key, so it appears just before the timestamp.
    session.publish_immutable(
        &session.path("roots").join(&active),
        &served_metadata.join(&active),
    )?;
    // Everything the timestamp leads to must survive a power loss before it does.
    for directory in [&served_targets, &served_metadata, mirror] {
        File::open(directory)?.sync_all()?;
    }
    session.replace(
        &metadata.join("timestamp.json"),
        &served_metadata.join("timestamp.json"),
    )?;
    if replace_descriptor {
        // Advertise a later trust root once a catalog that carries it is
        // current. An interrupted replacement leaves the earlier version,
        // which still verifies, and the next deployment replaces it.
        session.replace(&session.path(DESCRIPTOR), &mirror.join(DESCRIPTOR))?;
    }
    session.record(Some(&published))?;
    Ok(format!(
        "Published TUF version {version} for catalog {} at {}",
        session.catalog,
        mirror.display()
    ))
}

/// Whether the served descriptor must advance to a later trust root.
fn check_descriptor(
    session: &Session,
    served: &Path,
    next: &Descriptor,
    trust_root: &[u8],
) -> Result<bool> {
    match served.symlink_metadata() {
        Ok(metadata) => ensure!(
            metadata.is_file() && metadata.len() <= DESCRIPTOR_LIMIT,
            "invalid web root publisher.json"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let bytes = fs::read(served)?;
    let mut advertised: serde_json::Value =
        serde_json::from_slice(&bytes).context("invalid web root publisher.json")?;
    let mut expected = serde_json::to_value(next)?;
    let advertised_trust = advertised
        .as_object_mut()
        .and_then(|object| object.remove("trust_root"));
    expected
        .as_object_mut()
        .context("invalid descriptor")?
        .remove("trust_root");
    ensure!(
        advertised == expected,
        "the web root already advertises different publisher bootstrap material"
    );
    let advertised_trust = STANDARD
        .decode(
            advertised_trust
                .as_ref()
                .and_then(|value| value.as_str())
                .context("invalid advertised trust root")?,
        )
        .context("invalid advertised trust root")?;
    if advertised_trust == trust_root {
        return Ok(false);
    }
    // The current catalog's history holds both versions unchanged.
    read_current_catalog(&session.repo, &session.publisher, &advertised_trust)
        .context("the web root advertises a trust root outside the history of the catalog")?;
    let advertised_version = Pinned::parse(&advertised_trust)?.root().version;
    let version = Pinned::parse(trust_root)?.root().version;
    ensure!(
        version > advertised_version,
        "the web root advertises trust root version {advertised_version}; \
         pass that version or a later one as the trust root"
    );
    Ok(true)
}

/// Re-sign the timestamp, and the snapshot when it would expire first.
///
/// The deployed catalog stays served even when the workspace has published a
/// newer one that is not deployed yet. `catalog`, when given, must be the
/// deployed catalog.
pub(crate) fn refresh_mirror(
    publication: &Publication<'_>,
    catalog: Option<&str>,
) -> Result<String> {
    let session = Session::open(publication, catalog, true)?;
    let version = session.version();
    let mirror = &session.mirror;
    let served_metadata = mirror.join("metadata");
    // The record names the published snapshot. The web root is not trusted.
    let record = session
        .published
        .as_ref()
        .context("no completed deployment to refresh; deploy it with cps repo deploy")?;
    let active = format!("{}.root.json", session.root_version);
    ensure!(
        session.root_version == session.latest_root_version
            && fs::read(session.path("root.json")).ok()
                == fs::read(served_metadata.join(&active)).ok(),
        "publish the active TUF root with a full deployment before refreshing"
    );
    let snapshot_version = record.snapshot.version;
    let snapshot = served_metadata.join(format!("{snapshot_version}.snapshot.json"));
    ensure!(
        snapshot
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= ROLE_LIMIT)
            && sha256(&snapshot)? == record.snapshot.sha256,
        "the published snapshot {snapshot_version} differs from the publication record"
    );

    let staged = session.path("metadata");
    fs::create_dir(&staged)?;
    for entry in fs::read_dir(session.path("roots"))? {
        let entry = entry?;
        fs::copy(entry.path(), staged.join(entry.file_name()))?;
    }
    fs::copy(
        &snapshot,
        staged.join(snapshot.file_name().context("missing name")?),
    )?;
    // Copy each role listed by the recorded snapshot and check its digest.
    let snapshot_document = json(&snapshot, ROLE_LIMIT)?;
    let meta = snapshot_document["signed"]["meta"]
        .as_object()
        .context("invalid published snapshot")?;
    for (role, entry) in meta {
        let role_version = entry["version"].as_u64().unwrap_or(0);
        let role_sha256 = entry["hashes"]["sha256"].as_str().unwrap_or("");
        ensure!(
            role.strip_suffix(".json").is_some_and(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            }) && (1..=MAX_VERSION).contains(&role_version)
                && role_sha256.len() == 64,
            "unexpected role in the published snapshot"
        );
        let file = served_metadata.join(format!("{role_version}.{role}"));
        ensure!(
            file.symlink_metadata()
                .is_ok_and(|metadata| metadata.is_file())
                && sha256(&file)? == role_sha256,
            "the published {role} differs from the snapshot"
        );
        fs::copy(&file, staged.join(format!("{role_version}.{role}")))?;
    }
    let targets_version = meta
        .get("targets.json")
        .and_then(|entry| entry["version"].as_u64())
        .context("the published snapshot lists no targets metadata")?;
    let targets_expires = json(
        &served_metadata.join(format!("{targets_version}.targets.json")),
        ROLE_LIMIT,
    )?["signed"]["expires"]
        .as_str()
        .map(timestamp_of)
        .context("missing targets expiry")??;
    let now = jiff::Timestamp::now();
    ensure!(
        targets_expires > now,
        "the targets metadata expired at {targets_expires}; deploy the release with the targets key"
    );
    if targets_expires < now.checked_add(jiff::SignedDuration::try_from(TARGETS_WARNING)?)? {
        eprintln!(
            "Warning: the targets metadata expires at {targets_expires}. \
             Deploy the release with the targets key before then."
        );
    }

    let keys = fs::canonicalize(publication.keys)?;
    let timestamp_keys = role_keys(&keys, "timestamp", mirror)?;
    ensure!(!timestamp_keys.is_empty(), "missing timestamp key source");
    let snapshot_expires = timestamp_of(&record.snapshot.expires)?;
    let lifetime = publication.lifetimes.timestamp;
    let timestamp_expires = expiry_after(lifetime)?;
    let resign = snapshot_expires
        < now.checked_add(jiff::SignedDuration::try_from(lifetime + SNAPSHOT_MARGIN)?)?;
    let snapshot_keys = if resign {
        let keys = role_keys(&keys, "snapshot", mirror)?;
        ensure!(
            !keys.is_empty(),
            "the snapshot expires at {snapshot_expires}. Supply a snapshot key source to re-sign it"
        );
        keys
    } else {
        Vec::new()
    };
    let next_snapshot_expires = resign
        .then(|| expiry_after(publication.lifetimes.snapshot))
        .transpose()?;
    let refreshed = session.path("refreshed");
    refresh_metadata(RefreshMetadata {
        root: &session.path("root.json"),
        snapshot: &snapshot,
        output: &refreshed,
        timestamp_keys: &timestamp_keys,
        snapshot_keys: &snapshot_keys,
        version: NonZeroU64::new(version).context("invalid metadata version")?,
        snapshot_expires: next_snapshot_expires.as_deref(),
        timestamp_expires: &timestamp_expires,
    })?;
    for entry in fs::read_dir(&refreshed)? {
        let entry = entry?;
        fs::copy(entry.path(), staged.join(entry.file_name()))?;
    }
    session.verify(&staged, &mirror.join("targets"))?;

    let next_snapshot = refreshed.join(format!("{version}.snapshot.json"));
    let snapshot_record = match &next_snapshot_expires {
        Some(expires) => SnapshotRecord {
            version,
            sha256: sha256(&next_snapshot)?,
            expires: expires.clone(),
        },
        None => record.snapshot.clone(),
    };
    let published = Published {
        catalog: session.catalog.clone(),
        expires: timestamp_expires.clone(),
        sha256: sha256(&refreshed.join("timestamp.json"))?,
        snapshot: snapshot_record,
    };
    // Keep the previous record during the reservation so an interrupted refresh can be retried.
    session.record(Some(record))?;
    if resign {
        session.publish_immutable(
            &next_snapshot,
            &served_metadata.join(format!("{version}.snapshot.json")),
        )?;
    }
    File::open(&served_metadata)?.sync_all()?;
    session.replace(
        &refreshed.join("timestamp.json"),
        &served_metadata.join("timestamp.json"),
    )?;
    session.record(Some(&published))?;
    Ok(if resign {
        format!(
            "Refreshed TUF snapshot and timestamp version {version} for catalog {} at {}, valid until {timestamp_expires}",
            session.catalog,
            mirror.display()
        )
    } else {
        format!(
            "Refreshed TUF timestamp version {version} for catalog {} at {}, valid until {timestamp_expires}",
            session.catalog,
            mirror.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetimes_accept_short_and_long_units() -> Result<()> {
        assert_eq!(parse_lifetime("7d")?, days(7));
        assert_eq!(parse_lifetime("40 days")?, days(40));
        assert_eq!(parse_lifetime("1 hours")?, Duration::from_secs(3600));
        assert_eq!(parse_lifetime("12h")?, Duration::from_secs(12 * 3600));
        for invalid in ["", "0d", "07d", "7", "7 weeks", "10000d", "-1d", "d"] {
            assert!(parse_lifetime(invalid).is_err(), "{invalid}");
        }
        Ok(())
    }
}
