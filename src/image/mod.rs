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
    model::{FORMAT, Node, validate_publisher},
    repo::{Published, canonical_repo, read_key},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Settings {
    pub format: u32,
    pub id: String,
    pub revision: u64,
    pub arch: String,
    pub facets: BTreeMap<String, bool>,
    pub uid: u32,
    pub gid: u32,
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
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Installed {
    pub published: Published,
    pub explicit: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Managed {
    pub node: Node,
    pub owners: BTreeSet<String>,
    pub preserve: bool,
    pub implicit: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct State {
    pub settings: Settings,
    pub publisher: Publisher,
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

pub fn create_image(
    path: &Path,
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    arch: &str,
    facets: BTreeMap<String, bool>,
) -> Result<String> {
    validate_publisher(publisher)?;
    validate_publisher(arch).context("invalid architecture")?;
    for facet in facets.keys() {
        validate_publisher(facet).context("invalid facet")?;
    }
    let publisher = Publisher {
        name: publisher.to_owned(),
        repo: canonical_repo(repo)?,
        key: read_key(trust_key)?,
    };
    let mut random = [0; 16];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| anyhow::anyhow!("generating image identity"))?;
    random[6] = (random[6] & 0x0f) | 0x40;
    random[8] = (random[8] & 0x3f) | 0x80;
    let raw = hex::encode(random);
    let id = format!(
        "{}-{}-{}-{}-{}",
        &raw[..8],
        &raw[8..12],
        &raw[12..16],
        &raw[16..20],
        &raw[20..]
    );
    let settings = Settings {
        format: FORMAT,
        id: id.clone(),
        revision: 0,
        arch: arch.to_owned(),
        facets,
        uid: geteuid().as_raw(),
        gid: getegid().as_raw(),
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
        [serde_json::to_string(&publisher)?],
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
        let root = Root::open(path)?;
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
        let db = Connection::open_with_flags(
            Root::fd_path(&state_dir).join("state.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        configure(&db)?;
        Ok(Self {
            root,
            db,
            _state_dir: state_dir,
            _lock: lock,
        })
    }

    pub fn state(&self) -> Result<State> {
        let settings: Settings = decode(&self.db, "SELECT data FROM image WHERE id=1")?;
        ensure!(settings.format == FORMAT, "unsupported image format");
        Ok(State {
            settings,
            publisher: decode(&self.db, "SELECT data FROM publishers WHERE id=1")?,
            installed: decode_rows(&self.db, "SELECT name, data FROM installed")?,
            paths: decode_rows(&self.db, "SELECT path, data FROM paths")?,
        })
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

    pub fn record_pending(&self, data: &impl Serialize) -> Result<()> {
        self.db.execute(
            "INSERT INTO pending VALUES (1, ?1)",
            [serde_json::to_string(data)?],
        )?;
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
