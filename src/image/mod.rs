mod builds;
pub(crate) mod publishers;
pub(crate) mod root;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{Connection, OptionalExtension, params};
use rustix::{
    fs::{FlockOperation, flock},
    process::{getegid, geteuid},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use self::root::Root;
use crate::{
    attestation::BuildTrust,
    kdl_format::{PublisherConfig, read_image_definition},
    model::{FORMAT, Node, validate_name, validate_publisher},
    repo::{BuildReceipt, Published, canonical_repo, catalog_index, read_key},
};

// Earlier binaries must reject protected images instead of ignoring build trust.
const BUILD_TRUST_IMAGE_FORMAT: u32 = 2;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Settings {
    pub format: u32,
    pub id: String,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "publisher_generation_is_initial")]
    pub publisher_generation: u64,
    pub arch: String,
    pub facets: BTreeMap<String, bool>,
    pub uid: u32,
    pub gid: u32,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) created_directories: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_parent: Option<BootParent>,
    // Absent on images that predate tracking. Their proposals are discovered
    // from the saved vendor copies the first time they are reviewed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config_updates: Option<BTreeMap<String, u64>>,
}

pub(crate) fn publisher_generation_is_initial(generation: &u64) -> bool {
    *generation == 0
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BootParent {
    pub id: String,
    pub revision: u64,
}

impl Settings {
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
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum Publishers {
    Single(Publisher),
    Ordered(Vec<Publisher>),
    Profiled {
        release_profile: String,
        publishers: Vec<Publisher>,
    },
}

impl Publishers {
    fn new(publishers: Vec<Publisher>, profile: Option<String>) -> Result<Self> {
        let publishers = match profile {
            Some(release_profile) => Self::Profiled {
                release_profile,
                publishers,
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

    pub(crate) fn get(&self, name: &str) -> Option<&Publisher> {
        self.as_slice()
            .iter()
            .find(|publisher| publisher.name == name)
    }

    fn image_format(&self) -> u32 {
        if self
            .as_slice()
            .iter()
            .any(|publisher| publisher.build_trust.is_some())
        {
            BUILD_TRUST_IMAGE_FORMAT
        } else {
            FORMAT
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.as_slice().is_empty(),
            "an image requires at least one publisher"
        );
        if let Some(profile) = self.release_profile() {
            validate_name(profile).context("invalid image release profile")?;
        }
        let mut names = BTreeSet::new();
        for publisher in self.as_slice() {
            validate_publisher(&publisher.name)?;
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

pub struct Image {
    db: Connection,
    pub root: Root,
    _state_dir: File,
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
    initialize(path, Publishers::Single(publisher), arch, facets)
}

pub fn create_configured_image(path: &Path, config: &Path) -> Result<String> {
    let config = read_image_definition(config)?;
    let publishers = config
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
        .collect::<Result<Vec<_>>>()?;
    let publishers = Publishers::new(publishers, config.release_profile)?;
    let pins = publishers.release_pins(&config.arch, &config.facets, None)?;
    for publisher in publishers.as_slice() {
        if let Some(pin) = pins.get(&publisher.name) {
            catalog_index(&publisher.repo, &publisher.name, &publisher.key, Some(pin))
                .with_context(|| {
                    format!(
                        "reading pinned catalog {} for {}",
                        pin.sha256, publisher.name
                    )
                })?;
        }
    }
    initialize(path, publishers, &config.arch, config.facets)
}

fn initialize(
    path: &Path,
    publishers: Publishers,
    arch: &str,
    facets: BTreeMap<String, bool>,
) -> Result<String> {
    validate_publisher(arch).context("invalid architecture")?;
    for facet in facets.keys() {
        validate_publisher(facet).context("invalid facet")?;
    }
    let id = new_identity()?;
    let settings = Settings {
        format: publishers.image_format(),
        id: id.clone(),
        revision: 0,
        publisher_generation: 0,
        arch: arch.to_owned(),
        facets,
        uid: geteuid().as_raw(),
        gid: getegid().as_raw(),
        created_directories: BTreeSet::new(),
        boot_parent: None,
        config_updates: Some(BTreeMap::new()),
    };
    fs::create_dir_all(path)?;
    Root::open(path)?;
    ensure!(
        fs::read_dir(path)?.next().is_none(),
        "image-create requires an empty directory"
    );
    let state = tempfile::Builder::new()
        .prefix(".cps-init-")
        .tempdir_in(path)?;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(state.path().join("lock"))?
        .sync_all()?;
    fs::create_dir(state.path().join("stage"))?;
    File::open(state.path().join("stage"))?.sync_all()?;
    let mut db = Connection::open(state.path().join("state.sqlite"))?;
    configure(&db)?;
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE image (id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
         CREATE TABLE publishers (id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
         CREATE TABLE installed (name TEXT PRIMARY KEY, data TEXT NOT NULL);
         CREATE TABLE paths (path TEXT PRIMARY KEY, data TEXT NOT NULL);
         CREATE TABLE pending (id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
         CREATE TABLE history (plan TEXT PRIMARY KEY, revision INTEGER NOT NULL);",
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
    File::open(state.path())?.sync_all()?;
    fs::rename(state.path(), path.join(".cps"))?;
    File::open(path)?.sync_all()?;
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
        )
    }

    pub(crate) fn open_query(path: &Path) -> Result<Self> {
        Self::open_database(
            Root::open(path)?,
            false,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
    }

    pub(crate) fn open_for_apply(path: &Path) -> Result<Self> {
        let root = Root::open(path)?;
        crate::boot::check_package_write_root(&root)?;
        Self::open_database(root, true, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
    }

    fn open_database(root: Root, exclusive: bool, flags: rusqlite::OpenFlags) -> Result<Self> {
        let state_dir = root.dir(".cps").context("opening image state")?;
        let lock = root.file(".cps/lock")?;
        flock(
            &lock,
            if exclusive {
                FlockOperation::LockExclusive
            } else {
                FlockOperation::LockShared
            },
        )?;
        root.file(".cps/state.sqlite")?;
        let db =
            Connection::open_with_flags(Root::fd_path(&state_dir).join("state.sqlite"), flags)?;
        if flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE) {
            configure(&db)?;
        }
        Ok(Self {
            root,
            db,
            _state_dir: state_dir,
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

    pub(crate) fn settings(&self) -> Result<Settings> {
        let settings: Settings = decode(&self.db, "SELECT data FROM image WHERE id=1")?;
        ensure!(
            matches!(settings.format, FORMAT | BUILD_TRUST_IMAGE_FORMAT),
            "unsupported image format"
        );
        Ok(settings)
    }

    pub fn state(&self) -> Result<State> {
        let settings = self.settings()?;
        let publishers = self.publishers()?;
        ensure!(
            settings.format == publishers.image_format(),
            "image format does not match its build trust policy"
        );
        Ok(State {
            settings,
            publishers,
            installed: self.installed()?,
            paths: decode_rows(&self.db, "SELECT path, data FROM paths")?,
        })
    }

    pub fn fork_for_boot(&self, parent: BootParent) -> Result<Settings> {
        self.require_clean()?;
        let mut settings = self.settings()?;
        ensure!(
            settings.id == parent.id && parent.revision.checked_add(1) == Some(settings.revision),
            "staged image does not descend from the requested base"
        );
        settings.id = new_identity()?;
        settings.boot_parent = Some(parent);
        self.db.execute(
            "UPDATE image SET data=?1 WHERE id=1",
            [serde_json::to_string(&settings)?],
        )?;
        Ok(settings)
    }

    pub fn stage(&self) -> Result<File> {
        self.root.dir(".cps/stage")
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
            "image has an incomplete transaction; rerun apply with the same plan"
        );
        Ok(())
    }

    pub fn record_pending(
        &mut self,
        data: &impl Serialize,
        publishers: &Publishers,
        receipts: &[BuildReceipt],
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        builds::save(&tx, publishers, receipts)?;
        tx.execute(
            "INSERT INTO pending VALUES (1, ?1)",
            [serde_json::to_string(data)?],
        )?;
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

    pub fn commit(&mut self, plan: &str, state: &State) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute(
            "UPDATE image SET data=?1 WHERE id=1",
            [serde_json::to_string(&state.settings)?],
        )?;
        tx.execute("DELETE FROM installed", [])?;
        for (name, package) in &state.installed {
            tx.execute(
                "INSERT INTO installed VALUES (?1, ?2)",
                params![name, serde_json::to_string(package)?],
            )?;
        }
        builds::prune(&tx, state)?;
        tx.execute("DELETE FROM paths", [])?;
        for (path, managed) in &state.paths {
            tx.execute(
                "INSERT INTO paths VALUES (?1, ?2)",
                params![path, serde_json::to_string(managed)?],
            )?;
        }
        tx.execute(
            "INSERT INTO history VALUES (?1, ?2)",
            params![plan, state.settings.revision],
        )?;
        tx.execute("DELETE FROM pending WHERE id=1", [])?;
        tx.commit()?;
        Ok(())
    }
}
