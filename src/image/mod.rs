mod builds;
mod catalogs;
pub(crate) mod diff;
pub(crate) mod history;
pub(crate) mod lock;
pub(crate) mod publishers;
pub(crate) mod root;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, DirBuilder, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{Connection, OptionalExtension, params};
use rustix::{
    fs::{FlockOperation, RenameFlags, flock, renameat_with},
    process::{getegid, geteuid},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use self::root::{Fingerprint, Observation, Root};
use crate::{
    attestation::BuildTrust,
    file_caps::FileCapabilities,
    git_install::GitSource,
    kdl_format::{ImageDefinition, PublisherConfig, read_image_definition},
    model::{Node, validate_digest, validate_name, validate_publisher},
    repo::{BuildReceipt, CatalogCheckpoint, Published, canonical_repo, catalog_index, read_key},
};

pub(crate) const IMAGE_FORMAT: u32 = 1;
// Development databases also used format 1 but did not set an application ID.
const IMAGE_APPLICATION_ID: i32 = i32::from_be_bytes(*b"CPSI");
pub(crate) const STATE_DIRECTORY: &str = "var/lib/cps/image";
pub(crate) const STATE_ANCESTORS: &[&str] = &["var", "var/lib", "var/lib/cps"];
pub(crate) const CONFIG_UPDATES_DIRECTORY: &str = "var/lib/cps/image/config-updates";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileIntent {
    pub(crate) roots: BTreeSet<String>,
    pub(crate) local: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DormantConfig {
    pub publisher: String,
    pub identity: String,
    pub vendor: Node,
    pub local: Option<Node>,
    pub pending_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retained: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub format: u32,
    pub id: String,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "publisher_generation_is_initial")]
    pub publisher_generation: u64,
    pub arch: String,
    pub facets: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) mediators: BTreeMap<String, String>,
    pub uid: u32,
    pub gid: u32,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) created_directories: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_parent: Option<BootParent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) boot_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bootstrap_catalog: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) catalog_checkpoints: BTreeMap<String, CatalogCheckpoint>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) profile_packages: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) profile_intent: Option<ProfileIntent>,
    pub(crate) config_updates: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) dormant_configs: BTreeMap<String, DormantConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) freezes: BTreeMap<String, crate::plan::FrozenPackage>,
}

pub(crate) fn publisher_generation_is_initial(generation: &u64) -> bool {
    *generation == 0
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BootParent {
    pub id: String,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<u64>,
}

impl BootParent {
    pub(crate) fn image_revision(&self) -> u64 {
        self.source_revision.unwrap_or(self.revision)
    }
}

impl Settings {
    pub(crate) fn check_state_paths(&self, paths: &BTreeMap<String, Managed>) -> Result<()> {
        let directory = STATE_DIRECTORY;
        let prefix = format!("{directory}/");
        ensure!(
            !paths.contains_key(directory)
                && !paths
                    .range(prefix.clone()..)
                    .next()
                    .is_some_and(|(path, _)| path.starts_with(&prefix)),
            "reserved image state path {directory}"
        );
        for &ancestor in STATE_ANCESTORS {
            if let Some(managed) = paths.get(ancestor) {
                ensure!(
                    matches!(managed.node, Node::Dir { mode, uid, .. } if uid == self.uid && mode & 0o022 == 0),
                    "unsafe image state ancestor {ancestor}"
                );
            }
        }
        Ok(())
    }

    pub(crate) fn boot_revision(&self) -> u64 {
        self.boot_revision.unwrap_or(self.revision)
    }

    pub(crate) fn as_boot_parent(&self) -> BootParent {
        BootParent {
            id: self.id.clone(),
            revision: self.boot_revision(),
            source_revision: self.boot_revision.map(|_| self.revision),
        }
    }

    pub(crate) fn next_revision(&self) -> Result<u64> {
        self.revision
            .checked_add(1)
            .context("image revision exhausted")
    }

    pub(crate) fn check_profile_packages(
        &self,
        installed: &BTreeMap<String, Installed>,
    ) -> Result<()> {
        for (name, digest) in &self.profile_packages {
            validate_digest(digest).context("invalid signed profile manifest digest")?;
            ensure!(
                installed
                    .get(name)
                    .is_some_and(|package| &package.published.sha256 == digest),
                "{name} is protected by the installed signed profile; apply a signed profile update to change it"
            );
        }
        Ok(())
    }

    pub(crate) fn check_catalog(&self, publisher: &str, next: &CatalogCheckpoint) -> Result<()> {
        if let Some(previous) = self.catalog_checkpoints.get(publisher) {
            previous.check(next).with_context(|| {
                format!("checking installed catalog checkpoint for {publisher}")
            })?;
        }
        Ok(())
    }

    pub(crate) fn check_profile_intent(
        &self,
        installed: &BTreeMap<String, Installed>,
    ) -> Result<()> {
        ensure!(
            self.profile_packages.is_empty() == self.profile_intent.is_none(),
            "installed signed profiles require recorded package roots"
        );
        if let Some(intent) = &self.profile_intent {
            ensure!(
                !intent.roots.is_empty()
                    && intent
                        .roots
                        .iter()
                        .all(|name| self.profile_packages.contains_key(name))
                    && intent.local.iter().all(|name| installed.contains_key(name))
                    && installed.iter().all(|(name, package)| {
                        package.explicit
                            == (intent.roots.contains(name) || intent.local.contains(name))
                    }),
                "installed package marks differ from the recorded profile and local roots"
            );
        }
        Ok(())
    }

    pub fn directory(&self) -> Node {
        Node::Dir {
            mode: 0o755,
            uid: self.uid,
            gid: self.gid,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Publisher {
    pub name: String,
    pub repo: PathBuf,
    pub key: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) build_trust: Option<BuildTrust>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) git: Option<GitSource>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum Publishers {
    Single(Publisher),
    Ordered(Vec<Publisher>),
    Profiled {
        release_profile: String,
        publishers: Vec<Publisher>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signed_count: Option<usize>,
    },
}

impl Publishers {
    fn new(publishers: Vec<Publisher>, profile: Option<String>) -> Result<Self> {
        let publishers = match profile {
            Some(release_profile) => Self::Profiled {
                release_profile,
                publishers,
                signed_count: None,
            },
            None => match <[Publisher; 1]>::try_from(publishers) {
                Ok([publisher]) => Self::Single(publisher),
                Err(publishers) => Self::Ordered(publishers),
            },
        };
        publishers.validate()?;
        Ok(publishers)
    }

    pub(crate) fn as_slice(&self) -> &[Publisher] {
        match self {
            Self::Single(publisher) => std::slice::from_ref(publisher),
            Self::Ordered(publishers) => publishers,
            Self::Profiled { publishers, .. } => publishers,
        }
    }

    pub(crate) fn release_profile(&self) -> Option<&str> {
        match self {
            Self::Profiled {
                release_profile, ..
            } => Some(release_profile),
            _ => None,
        }
    }

    pub(crate) fn signed_publishers(&self) -> &[Publisher] {
        match self {
            Self::Profiled {
                publishers,
                signed_count,
                ..
            } => &publishers[..signed_count.unwrap_or(publishers.len())],
            _ => &[],
        }
    }

    pub(crate) fn is_local(&self, name: &str) -> bool {
        self.release_profile().is_some()
            && self.as_slice()[self.signed_publishers().len()..]
                .iter()
                .any(|publisher| publisher.name == name)
    }

    fn with_publishers(&self, publishers: Vec<Publisher>) -> Result<Self> {
        let Some(profile) = self.release_profile() else {
            return Self::new(publishers, None);
        };
        let count = self.signed_publishers().len();
        let result = Self::Profiled {
            release_profile: profile.to_owned(),
            signed_count: (count != publishers.len()).then_some(count),
            publishers,
        };
        result.validate()?;
        Ok(result)
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Publisher> {
        self.as_slice()
            .iter()
            .find(|publisher| publisher.name == name)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.as_slice().is_empty(),
            "an image requires at least one publisher"
        );
        if let Some(profile) = self.release_profile() {
            validate_name(profile).context("invalid image release profile")?;
        }
        if let Self::Profiled {
            signed_count: Some(count),
            publishers,
            ..
        } = self
        {
            ensure!(
                *count > 0 && *count < publishers.len(),
                "invalid signed publisher count"
            );
        }
        let mut names = BTreeSet::new();
        for publisher in self.as_slice() {
            validate_publisher(&publisher.name)?;
            if let Some(git) = &publisher.git {
                ensure!(
                    publisher.name == git.publisher(),
                    "invalid Git publisher identity"
                );
            }
            if let Some(trust) = &publisher.build_trust {
                trust.validate()?;
                trust.check_publisher_key(&publisher.key)?;
            }
            ensure!(
                names.insert(&publisher.name),
                "duplicate publisher {}",
                publisher.name
            );
        }
        Ok(())
    }
}

impl Publisher {
    fn from_config(name: &str, repo: &Path, key: &Path) -> Result<Self> {
        validate_publisher(name)?;
        Ok(Self {
            name: name.to_owned(),
            repo: canonical_repo(repo)?,
            key: read_key(key)?,
            build_trust: None,
            git: None,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Installed {
    pub published: Published,
    pub explicit: bool,
}

impl Installed {
    pub(crate) fn verify(&self, name: &str, publishers: &Publishers) -> Result<()> {
        let manifest = &self.published.manifest;
        ensure!(
            manifest.name == name && publishers.get(&manifest.publisher).is_some(),
            "invalid installed package identity for {name}"
        );
        self.published.verify()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Managed {
    pub node: Node,
    pub owners: BTreeSet<String>,
    pub preserve: bool,
    pub implicit: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) original_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct State {
    pub settings: Settings,
    #[serde(rename = "publisher")]
    pub(crate) publishers: Publishers,
    pub installed: BTreeMap<String, Installed>,
    pub paths: BTreeMap<String, Managed>,
}

/// A next state recorded as changes to the committed state it replaces.
///
/// A pending transaction always applies to the committed revision it was
/// planned against: the committed tables cannot change while it is pending.
/// Publishers cannot change in a transaction, so the delta omits them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StateDelta {
    pub settings: Settings,
    /// New or changed packages, or `null` for removed packages.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) installed: BTreeMap<String, Option<Installed>>,
    /// New or changed managed paths, or `null` for removed paths.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) paths: BTreeMap<String, Option<Managed>>,
}

fn changes<T: Clone + PartialEq>(
    before: &BTreeMap<String, T>,
    after: &BTreeMap<String, T>,
) -> BTreeMap<String, Option<T>> {
    let mut changes = BTreeMap::new();
    for (key, value) in after {
        if before.get(key) != Some(value) {
            changes.insert(key.clone(), Some(value.clone()));
        }
    }
    for key in before.keys() {
        if !after.contains_key(key) {
            changes.insert(key.clone(), None);
        }
    }
    changes
}

fn apply_changes<T: Clone + PartialEq>(
    base: &mut BTreeMap<String, T>,
    changes: &BTreeMap<String, Option<T>>,
    kind: &str,
) -> Result<()> {
    for (key, change) in changes {
        match change {
            Some(value) => ensure!(
                base.insert(key.clone(), value.clone()).as_ref() != Some(value),
                "pending {kind} change for {key} does not change the committed state"
            ),
            None => ensure!(
                base.remove(key).is_some(),
                "pending {kind} removal of {key} is absent from the committed state"
            ),
        }
    }
    Ok(())
}

impl StateDelta {
    pub(crate) fn between(base: &State, next: &State) -> Self {
        Self {
            settings: next.settings.clone(),
            installed: changes(&base.installed, &next.installed),
            paths: changes(&base.paths, &next.paths),
        }
    }

    /// Rebuild the full next state on top of the committed state.
    pub(crate) fn apply(&self, base: &State) -> Result<State> {
        let mut next = base.clone();
        next.settings = self.settings.clone();
        apply_changes(&mut next.installed, &self.installed, "package")?;
        apply_changes(&mut next.paths, &self.paths, "path")?;
        Ok(next)
    }
}

/// A recorded fingerprint of a managed regular file and what CPS learned
/// from its verified contents.
#[derive(Clone, Debug)]
pub(crate) struct FileRecord {
    pub(crate) observation: Observation,
    /// Cached ELF metadata as JSON, when known.
    pub(crate) elf: Option<String>,
}

pub(crate) type FileRecords = BTreeMap<String, FileRecord>;

const FILE_COLUMNS: &str =
    "path, sha256, capabilities, ino, size, mtime_ns, ctime_ns, mode, uid, gid, elf";

// SQLite stores 64-bit signed integers; inode numbers and sizes keep their bits.
#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn file_record(row: &rusqlite::Row<'_>) -> Result<(String, FileRecord)> {
    let capabilities: Option<String> = row.get(2)?;
    let capabilities = capabilities
        .map(|json| serde_json::from_str::<FileCapabilities>(&json))
        .transpose()?;
    let fingerprint = Fingerprint {
        ino: row.get::<_, i64>(3)? as u64,
        size: row.get::<_, i64>(4)? as u64,
        mtime_ns: row.get(5)?,
        ctime_ns: row.get(6)?,
        mode: row.get(7)?,
        uid: row.get(8)?,
        gid: row.get(9)?,
    };
    Ok((
        row.get(0)?,
        FileRecord {
            observation: Observation {
                fingerprint,
                sha256: row.get(1)?,
                capabilities,
            },
            elf: row.get(10)?,
        },
    ))
}

#[allow(clippy::cast_possible_wrap)]
fn save_file_records(
    tx: &rusqlite::Transaction<'_>,
    observed: &BTreeMap<String, Observation>,
    stale: &BTreeSet<String>,
) -> Result<()> {
    let mut delete = tx.prepare_cached("DELETE FROM files WHERE path=?1")?;
    for path in stale {
        delete.execute([path])?;
    }
    let mut insert = tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO files ({FILE_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL)"
    ))?;
    for (path, observation) in observed {
        let fingerprint = &observation.fingerprint;
        insert.execute(params![
            path,
            observation.sha256,
            observation
                .capabilities
                .map(|capabilities| serde_json::to_string(&capabilities))
                .transpose()?,
            fingerprint.ino as i64,
            fingerprint.size as i64,
            fingerprint.mtime_ns,
            fingerprint.ctime_ns,
            fingerprint.mode,
            fingerprint.uid,
            fingerprint.gid,
        ])?;
    }
    Ok(())
}

pub struct Image {
    db: Connection,
    pub root: Root,
    pub(crate) state_dir: Root,

    _lock: File,
}

fn configure(db: &Connection) -> Result<()> {
    db.pragma_update(None, "synchronous", "EXTRA")?;
    Ok(())
}

fn new_identity() -> Result<String> {
    let mut random = [0; 16];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| anyhow::anyhow!("generating image identity"))?;
    random[6] = (random[6] & 0x0f) | 0x40;
    random[8] = (random[8] & 0x3f) | 0x80;
    let raw = hex::encode(random);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &raw[..8],
        &raw[8..12],
        &raw[12..16],
        &raw[16..20],
        &raw[20..]
    ))
}

pub fn create_image(
    path: &Path,
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    arch: &str,
    facets: BTreeMap<String, bool>,
) -> Result<String> {
    let publisher = Publisher::from_config(publisher, repo, trust_key)?;
    initialize(
        path,
        Publishers::Single(publisher),
        arch,
        facets,
        None,
        None,
    )
}

pub fn create_configured_image(path: &Path, config: &Path) -> Result<String> {
    create_configured_image_at(path, config, None, None)
}

/// Initialize a configured image at an explicit historical or candidate catalog.
pub fn create_configured_image_at(
    path: &Path,
    config: &Path,
    catalog: Option<&str>,
    primary_repo: Option<&Path>,
) -> Result<String> {
    let mut config = read_image_definition(config)?;
    if let Some(repo) = primary_repo {
        ensure!(
            catalog.is_some(),
            "a repository override requires an explicit bootstrap catalog"
        );
        config
            .publishers
            .first_mut()
            .context("missing primary publisher")?
            .repository = repo.to_owned();
    }
    if let Some(catalog) = catalog {
        crate::model::validate_digest(catalog)?;
        ensure!(
            config.release_profile.is_some(),
            "a bootstrap catalog requires a release profile"
        );
        ensure!(
            config
                .bootstrap_catalog
                .as_deref()
                .is_none_or(|configured| configured == catalog),
            "explicit bootstrap catalog differs from the image configuration"
        );
        config.bootstrap_catalog = Some(catalog.to_owned());
    }
    let publishers = configured_publishers(&config)?;
    let publishers = Publishers::new(publishers, config.release_profile)?;
    let profile = publishers.select_profile(
        &config.arch,
        &config.facets,
        config.bootstrap_catalog.as_deref(),
        None,
    )?;
    if config.bootstrap_catalog.is_some() {
        ensure!(
            profile
                .as_ref()
                .is_some_and(|profile| !profile.record.composition.is_empty()),
            "bootstrap catalog has no recorded signed composition for the profile"
        );
    }
    for publisher in publishers.as_slice() {
        if let Some(pin) = profile
            .as_ref()
            .and_then(|profile| profile.pin(&publisher.name))
        {
            catalog_index(&publisher.repo, &publisher.name, &publisher.key, Some(pin))
                .with_context(|| {
                    format!(
                        "reading pinned catalog {} for {}",
                        pin.sha256, publisher.name
                    )
                })?;
        }
    }
    initialize(
        path,
        publishers,
        &config.arch,
        config.facets,
        config.bootstrap_catalog,
        config.image_id,
    )
}

fn configured_publishers(config: &ImageDefinition) -> Result<Vec<Publisher>> {
    config
        .publishers
        .iter()
        .map(
            |PublisherConfig {
                 name,
                 repository,
                 trust_key,
                 build_trust,
             }| {
                let mut publisher = Publisher::from_config(name, repository, trust_key)?;
                publisher.build_trust = build_trust
                    .as_ref()
                    .map(|config| config.load(&publisher.key))
                    .transpose()
                    .with_context(|| format!("loading build trust for {name}"))?;
                Ok(publisher)
            },
        )
        .collect()
}

fn initialize(
    path: &Path,
    publishers: Publishers,
    arch: &str,
    facets: BTreeMap<String, bool>,
    bootstrap_catalog: Option<String>,
    image_id: Option<String>,
) -> Result<String> {
    validate_publisher(arch).context("invalid architecture")?;
    for facet in facets.keys() {
        validate_publisher(facet).context("invalid facet")?;
    }
    let id = image_id.map_or_else(new_identity, Ok)?;
    let settings = Settings {
        format: IMAGE_FORMAT,
        id: id.clone(),
        revision: 0,
        publisher_generation: 0,
        arch: arch.to_owned(),

        facets,
        mediators: BTreeMap::new(),
        uid: geteuid().as_raw(),
        gid: getegid().as_raw(),
        created_directories: BTreeSet::new(),
        boot_parent: None,
        boot_revision: None,
        bootstrap_catalog,
        catalog_checkpoints: BTreeMap::new(),
        profile_packages: BTreeMap::new(),
        profile_intent: None,
        config_updates: BTreeMap::new(),
        dormant_configs: BTreeMap::new(),
        freezes: BTreeMap::new(),
    };
    fs::create_dir_all(path)?;
    let root = Root::open(path)?;
    ensure!(
        fs::read_dir(path)?.next().is_none(),
        "image-create requires an empty directory"
    );
    let scratch = tempfile::Builder::new()
        .prefix(".cps-init-")
        .tempdir_in(path)?;
    for ancestor in STATE_ANCESTORS {
        let directory = scratch.path().join(ancestor);
        fs::create_dir(&directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o755))?;
    }
    let state = scratch.path().join(STATE_DIRECTORY);
    DirBuilder::new().mode(0o700).create(&state)?;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(state.join("lock"))?
        .sync_all()?;
    fs::create_dir(state.join("stage"))?;
    File::open(state.join("stage"))?.sync_all()?;
    let mut db = Connection::open(state.join("state.sqlite"))?;
    configure(&db)?;
    let tx = db.transaction()?;
    tx.pragma_update(None, "application_id", IMAGE_APPLICATION_ID)?;
    tx.execute_batch(
        "CREATE TABLE image (id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
         CREATE TABLE publishers (id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
         CREATE TABLE installed (name TEXT PRIMARY KEY, data TEXT NOT NULL);
         CREATE TABLE paths (path TEXT PRIMARY KEY, data TEXT NOT NULL);
         CREATE TABLE files (path TEXT PRIMARY KEY, sha256 TEXT NOT NULL, capabilities TEXT,
             ino INTEGER NOT NULL, size INTEGER NOT NULL, mtime_ns INTEGER NOT NULL,
             ctime_ns INTEGER NOT NULL, mode INTEGER NOT NULL, uid INTEGER NOT NULL,
             gid INTEGER NOT NULL, elf TEXT) WITHOUT ROWID;
         CREATE TABLE pending (id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
         CREATE TABLE history (plan TEXT PRIMARY KEY, revision INTEGER NOT NULL);
         CREATE TABLE transaction_details (plan TEXT PRIMARY KEY, data TEXT NOT NULL, sha256 TEXT NOT NULL);
         CREATE TABLE catalog_archives (publisher TEXT PRIMARY KEY, catalog BLOB NOT NULL);",
    )?;
    tx.execute(
        "INSERT INTO image VALUES (1, ?1)",
        [serde_json::to_string(&settings)?],
    )?;
    tx.execute(
        "INSERT INTO publishers VALUES (1, ?1)",
        [serde_json::to_string(&publishers)?],
    )?;
    tx.commit()?;
    drop(db);
    File::open(state)?.sync_all()?;
    for ancestor in STATE_ANCESTORS.iter().rev() {
        File::open(scratch.path().join(ancestor))?.sync_all()?;
    }
    let entry = STATE_ANCESTORS[0];
    let source = Root::open(scratch.path())?;
    renameat_with(
        source.as_file(),
        entry,
        root.as_file(),
        entry,
        RenameFlags::NOREPLACE,
    )
    .context("publishing image state")?;
    root.as_file().sync_all()?;
    Ok(id)
}

fn decode<T: DeserializeOwned>(db: &Connection, sql: &str) -> Result<T> {
    Ok(serde_json::from_str(
        &db.query_row(sql, [], |row| row.get::<_, String>(0))?,
    )?)
}

fn decode_rows<T: DeserializeOwned>(db: &Connection, sql: &str) -> Result<BTreeMap<String, T>> {
    let mut statement = db.prepare(sql)?;
    let mut rows = statement.query([])?;
    let mut records = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let value: String = row.get(1)?;
        records.insert(row.get(0)?, serde_json::from_str(&value)?);
    }
    Ok(records)
}

impl Image {
    pub fn open(path: &Path, exclusive: bool) -> Result<Self> {
        Self::open_database(
            Root::open(path)?,
            exclusive,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            false,
        )
    }

    pub(crate) fn open_query(path: &Path) -> Result<Self> {
        Self::open_query_root(Root::open(path)?)
    }

    pub(crate) fn open_query_root(root: Root) -> Result<Self> {
        Self::open_database(
            root,
            false,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            false,
        )
    }

    pub(crate) fn open_for_apply(path: &Path) -> Result<Self> {
        Self::open_database(
            Root::open(path)?,
            true,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            true,
        )
    }

    pub(crate) fn open_for_live_apply(root: Root, id: &str, revision: u64) -> Result<Self> {
        let image = Self::open_database(
            root,
            true,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            false,
        )?;
        let settings = image.settings()?;
        ensure!(
            settings.id == id && settings.boot_revision() == revision,
            "the running image differs from its boot entry"
        );
        Ok(image)
    }

    fn open_database(
        root: Root,
        exclusive: bool,
        flags: rusqlite::OpenFlags,
        package_write: bool,
    ) -> Result<Self> {
        let state_dir = root.open_image_state()?;
        let lock = state_dir.file("lock")?;
        flock(
            &lock,
            if exclusive {
                FlockOperation::LockExclusive
            } else {
                FlockOperation::LockShared
            },
        )?;
        if package_write {
            crate::boot::check_package_write_root(&root)?;
        }
        state_dir.file("state.sqlite")?;
        let db = Connection::open_with_flags(
            Root::fd_path(state_dir.as_file()).join("state.sqlite"),
            flags,
        )?;
        let application_id =
            db.pragma_query_value(None, "application_id", |row| row.get::<_, i32>(0))?;
        ensure!(
            application_id == IMAGE_APPLICATION_ID,
            "unsupported image database layout"
        );
        if flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE) {
            configure(&db)?;
        }
        Ok(Self {
            root,
            db,
            state_dir,

            _lock: lock,
        })
    }

    pub(crate) fn publishers(&self) -> Result<Publishers> {
        let publishers: Publishers = decode(&self.db, "SELECT data FROM publishers WHERE id=1")?;
        publishers.validate()?;
        Ok(publishers)
    }

    pub(crate) fn installed(&self) -> Result<BTreeMap<String, Installed>> {
        decode_rows(&self.db, "SELECT name, data FROM installed")
    }

    pub(crate) fn installed_package(&self, name: &str) -> Result<Option<Installed>> {
        let bytes: Option<String> = self
            .db
            .query_row("SELECT data FROM installed WHERE name=?1", [name], |row| {
                row.get(0)
            })
            .optional()?;
        bytes
            .map(|bytes| Ok(serde_json::from_str(&bytes)?))
            .transpose()
    }

    pub(crate) fn managed_path(&self, path: &str) -> Result<Option<Managed>> {
        let bytes: Option<String> = self
            .db
            .prepare_cached("SELECT data FROM paths WHERE path=?1")?
            .query_row([path], |row| row.get(0))
            .optional()?;
        bytes
            .map(|bytes| Ok(serde_json::from_str(&bytes)?))
            .transpose()
    }

    pub(crate) fn settings(&self) -> Result<Settings> {
        let settings: Settings = decode(&self.db, "SELECT data FROM image WHERE id=1")?;
        ensure!(settings.format == IMAGE_FORMAT, "unsupported image format");
        ensure!(
            settings
                .boot_revision
                .is_none_or(|revision| revision < settings.revision)
                && settings.boot_parent.as_ref().is_none_or(|parent| {
                    parent.source_revision.is_none_or(|revision| {
                        parent.revision < revision && revision < settings.revision
                    })
                }),
            "invalid boot revision"
        );
        Ok(settings)
    }

    pub fn state(&self) -> Result<State> {
        let settings = self.settings()?;
        let publishers = self.publishers()?;
        ensure!(
            settings.bootstrap_catalog.is_none()
                || (publishers.release_profile().is_some()
                    && settings.catalog_checkpoints.is_empty()),
            "bootstrap catalog requires an unprovisioned release profile"
        );
        ensure!(
            settings.profile_packages.is_empty()
                || (publishers.release_profile().is_some()
                    && settings.bootstrap_catalog.is_none()
                    && !settings.catalog_checkpoints.is_empty()),
            "protected packages require an installed signed profile"
        );
        ensure!(
            !matches!(
                publishers,
                Publishers::Profiled {
                    signed_count: Some(_),
                    ..
                }
            ) || !settings.profile_packages.is_empty()
                || (settings.revision == 0 && settings.bootstrap_catalog.is_some()),
            "local publishers require an installed signed profile composition"
        );
        for (publisher, checkpoint) in &settings.catalog_checkpoints {
            ensure!(
                publishers.get(publisher).is_some(),
                "checkpoint for unconfigured publisher {publisher}"
            );
            validate_digest(&checkpoint.catalog).context("invalid catalog checkpoint digest")?;
        }
        let paths = decode_rows(&self.db, "SELECT path, data FROM paths")?;
        let mut dormant_identities = BTreeSet::new();
        for (path, config) in &settings.dormant_configs {
            crate::model::validate_path(path)?;
            ensure!(
                (config.retained || publishers.release_profile().is_none())
                    && (!config.retained || config.local.is_some())
                    && publishers.get(&config.publisher).is_some()
                    && !paths.contains_key(path)
                    && dormant_identities.insert((&config.publisher, &config.identity))
                    && matches!(
                        &config.vendor,
                        Node::File {
                            capabilities: None,
                            ..
                        }
                    )
                    && config.local.as_ref().is_none_or(|local| matches!(
                        local,
                        Node::File {
                            capabilities: None,
                            ..
                        }
                    ))
                    && config
                        .pending_revision
                        .is_none_or(|revision| (1..=settings.revision).contains(&revision)),
                "invalid disabled configuration record for {path}"
            );
        }
        let installed = self.installed()?;
        settings.check_profile_packages(&installed)?;
        settings.check_profile_intent(&installed)?;
        crate::plan::freeze::check(&settings.freezes, &installed)?;
        Ok(State {
            settings,
            publishers,
            installed,
            paths,
        })
    }

    pub fn fork_for_boot(&self, parent: BootParent) -> Result<Settings> {
        self.require_clean()?;
        let mut settings = self.settings()?;
        ensure!(
            settings.id == parent.id
                && parent.image_revision().checked_add(1) == Some(settings.revision),
            "staged image does not descend from the requested base"
        );
        settings.id = new_identity()?;
        settings.boot_parent = Some(parent);
        settings.boot_revision = None;
        self.db.execute(
            "UPDATE image SET data=?1 WHERE id=1",
            [serde_json::to_string(&settings)?],
        )?;
        Ok(settings)
    }

    pub fn stage(&self) -> Result<Root> {
        self.state_dir.subdir("stage")
    }

    pub(crate) fn archived_build(&self, manifest: &str) -> Result<Option<(BuildReceipt, Vec<u8>)>> {
        if !builds::exists(&self.db)? {
            return Ok(None);
        }
        builds::read(&self.db, manifest)
    }

    pub fn pending<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        let record = self
            .db
            .query_row("SELECT data FROM pending WHERE id=1", [], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        record
            .map(|json| Ok(serde_json::from_str(&json)?))
            .transpose()
    }

    pub fn require_clean(&self) -> Result<()> {
        let pending: bool =
            self.db
                .query_row("SELECT EXISTS(SELECT 1 FROM pending)", [], |row| row.get(0))?;
        ensure!(
            !pending,
            "image has an incomplete transaction; use transaction status or rerun apply with the same plan"
        );
        Ok(())
    }

    pub(crate) fn verify_catalog_archives(&self, state: &State) -> Result<()> {
        catalogs::verify(&self.db, state)
    }

    pub fn record_pending(
        &mut self,
        data: &impl Serialize,
        next: &State,
        receipts: &[BuildReceipt],
        repo_backed: bool,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        if repo_backed {
            catalogs::save(&tx, next)?;
        }
        catalogs::verify(&tx, next)?;
        builds::save(&tx, &next.publishers, receipts)?;
        tx.execute(
            "INSERT INTO pending VALUES (1, ?1)",
            [serde_json::to_string(data)?],
        )?;
        Ok(tx.commit()?)
    }

    /// Load recorded file fingerprints, for every path or only selected ones.
    pub(crate) fn file_records(&self, selected: Option<&BTreeSet<&str>>) -> Result<FileRecords> {
        let mut records = BTreeMap::new();
        if let Some(selected) = selected {
            let mut statement = self
                .db
                .prepare_cached(&format!("SELECT {FILE_COLUMNS} FROM files WHERE path=?1"))?;
            for path in selected {
                let mut rows = statement.query([path])?;
                if let Some(row) = rows.next()? {
                    let (path, record) = file_record(row)?;
                    records.insert(path, record);
                }
            }
        } else {
            let mut statement = self
                .db
                .prepare(&format!("SELECT {FILE_COLUMNS} FROM files"))?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let (path, record) = file_record(row)?;
                records.insert(path, record);
            }
        }
        Ok(records)
    }

    /// Record new settled fingerprints and forget ones that no longer match.
    /// Fingerprints describe the files on disk, not the committed state, so
    /// they can be saved outside a transaction's commit.
    pub(crate) fn record_files(
        &mut self,
        observed: &BTreeMap<String, Observation>,
        stale: &BTreeSet<String>,
    ) -> Result<()> {
        if observed.is_empty() && stale.is_empty() {
            return Ok(());
        }
        let tx = self.db.transaction()?;
        save_file_records(&tx, observed, stale)?;
        Ok(tx.commit()?)
    }

    /// Cache ELF metadata for files whose recorded fingerprint is unchanged.
    #[allow(clippy::cast_possible_wrap)]
    pub(crate) fn record_elf(&mut self, entries: &[(&str, &Observation, String)]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let tx = self.db.transaction()?;
        {
            let mut update = tx.prepare_cached(
                "UPDATE files SET elf=?1 WHERE path=?2 AND sha256=?3 AND ino=?4 AND size=?5
                 AND mtime_ns=?6 AND ctime_ns=?7 AND mode=?8 AND uid=?9 AND gid=?10",
            )?;
            for (path, observation, elf) in entries {
                let fingerprint = &observation.fingerprint;
                update.execute(params![
                    elf,
                    path,
                    observation.sha256,
                    fingerprint.ino as i64,
                    fingerprint.size as i64,
                    fingerprint.mtime_ns,
                    fingerprint.ctime_ns,
                    fingerprint.mode,
                    fingerprint.uid,
                    fingerprint.gid,
                ])?;
            }
        }
        Ok(tx.commit()?)
    }

    pub fn update_pending(&self, data: &impl Serialize) -> Result<()> {
        let changed = self.db.execute(
            "UPDATE pending SET data=?1 WHERE id=1",
            [serde_json::to_string(data)?],
        )?;
        ensure!(changed == 1, "no pending transaction to update");
        Ok(())
    }

    pub fn applied(&self, plan: &str) -> Result<Option<u64>> {
        Ok(self
            .db
            .query_row(
                "SELECT revision FROM history WHERE plan=?1",
                [plan],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Commit a pending transaction by applying its recorded delta.
    ///
    /// Only changed package and path rows are written, in one SQLite
    /// transaction that also records history and clears the pending record.
    /// `written` lists every path the transaction wrote, whose recorded
    /// fingerprints are dropped; `observed` adds new settled fingerprints of
    /// files the transaction verified but did not write.
    pub(crate) fn commit(
        &mut self,
        plan: &str,
        delta: &StateDelta,
        next: &State,
        written: &BTreeSet<&str>,
        observed: &BTreeMap<String, Observation>,
        details: &history::TransactionDetails,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute(
            "UPDATE image SET data=?1 WHERE id=1",
            [serde_json::to_string(&delta.settings)?],
        )?;
        {
            let mut upsert = tx.prepare("INSERT OR REPLACE INTO installed VALUES (?1, ?2)")?;
            let mut delete = tx.prepare("DELETE FROM installed WHERE name=?1")?;
            for (name, package) in &delta.installed {
                match package {
                    Some(package) => {
                        upsert.execute(params![name, serde_json::to_string(package)?])?;
                    }
                    None => ensure!(
                        delete.execute([name])? == 1,
                        "committed package {name} disappeared"
                    ),
                }
            }
        }
        builds::prune(&tx, next)?;
        {
            let mut upsert = tx.prepare("INSERT OR REPLACE INTO paths VALUES (?1, ?2)")?;
            let mut delete = tx.prepare("DELETE FROM paths WHERE path=?1")?;
            for (path, managed) in &delta.paths {
                match managed {
                    Some(managed) => {
                        upsert.execute(params![path, serde_json::to_string(managed)?])?;
                    }
                    None => ensure!(
                        delete.execute([path])? == 1,
                        "committed path {path} disappeared"
                    ),
                }
            }
        }
        let forgotten: BTreeSet<String> = delta
            .paths
            .keys()
            .map(String::as_str)
            .chain(written.iter().copied())
            .map(str::to_owned)
            .collect();
        let observed: BTreeMap<_, _> = observed
            .iter()
            .filter(|(path, _)| {
                !forgotten.contains(path.as_str()) && next.paths.contains_key(path.as_str())
            })
            .map(|(path, observation)| (path.clone(), observation.clone()))
            .collect();
        save_file_records(&tx, &observed, &forgotten)?;
        tx.execute(
            "INSERT INTO history VALUES (?1, ?2)",
            params![plan, next.settings.revision],
        )?;
        history::record(&tx, plan, next.settings.revision, details)?;
        tx.execute("DELETE FROM pending WHERE id=1", [])?;
        tx.commit()?;
        Ok(())
    }
}
