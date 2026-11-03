use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::{
    image::root::Root,
    model::{Action, FORMAT, Manifest, Node, digest, validate_digest},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    format: u32,
    publisher: String,
    entries: BTreeMap<String, String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SignedCatalog {
    catalog: Catalog,
    signature: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Published {
    pub sha256: String,
    pub manifest: Manifest,
}

pub struct Snapshot {
    pub sha256: String,
    pub packages: Vec<Published>,
}

pub fn read_key(path: &Path) -> Result<Vec<u8>> {
    let bytes = fs::read(path).with_context(|| format!("reading trust key {}", path.display()))?;
    // SubjectPublicKeyInfo for Ed25519, as emitted by openssl pkey -pubout -outform DER.
    const PREFIX: &[u8] = b"\x30\x2a\x30\x05\x06\x03\x2b\x65\x70\x03\x21\x00";
    let key = bytes
        .strip_prefix(PREFIX)
        .context("expected an Ed25519 DER public key")?;
    ensure!(key.len() == 32, "invalid Ed25519 public key length");
    Ok(key.to_vec())
}

fn read_current(repo: &Path) -> Result<String> {
    let pointer =
        fs::read_to_string(repo.join("current")).context("reading repository current catalog")?;
    let hash = pointer.trim_end();
    validate_digest(hash)?;
    Ok(hash.to_owned())
}

fn read_object(repo: &Path, kind: &str, hash: &str) -> Result<Vec<u8>> {
    validate_digest(hash)?;
    let path = repo.join(kind).join(hash);
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        digest(&bytes) == hash,
        "digest mismatch for {}",
        path.display()
    );
    Ok(bytes)
}

fn read_catalog(repo: &Path, hash: &str, publisher: &str, key: &[u8]) -> Result<Catalog> {
    let signed: SignedCatalog = serde_json::from_slice(&read_object(repo, "catalogs", hash)?)?;
    let catalog = signed.catalog;
    ensure!(catalog.format == FORMAT, "unsupported catalog format");
    ensure!(
        catalog.publisher == publisher,
        "unexpected publisher {}",
        catalog.publisher
    );
    UnparsedPublicKey::new(&ED25519, key)
        .verify(
            &serde_json::to_vec(&catalog)?,
            &hex::decode(signed.signature)?,
        )
        .map_err(|_| anyhow::anyhow!("invalid signature on catalog {hash}"))?;
    Ok(catalog)
}

pub fn snapshot(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    pinned: Option<&str>,
) -> Result<Snapshot> {
    let hash = match pinned {
        Some(hash) => hash.to_owned(),
        None => read_current(repo)?,
    };
    let catalog = read_catalog(repo, &hash, publisher, key)?;
    let mut packages = Vec::with_capacity(catalog.entries.len());
    for (id, sha256) in catalog.entries {
        let bytes = read_object(repo, "manifests", &sha256)?;
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        ensure!(
            serde_json::to_vec(&manifest)? == bytes,
            "noncanonical manifest {id}"
        );
        manifest.validate(true)?;
        ensure!(
            manifest.id() == id && manifest.publisher == publisher,
            "manifest identity mismatch for {id}"
        );
        packages.push(Published { sha256, manifest });
    }
    Ok(Snapshot {
        sha256: hash,
        packages,
    })
}

fn write_atomic(path: &Path, bytes: &[u8], immutable: bool) -> Result<()> {
    let parent = path.parent().context("missing object parent")?;
    if immutable && path.try_exists()? {
        ensure!(
            fs::read(path)? == bytes,
            "immutable object differs at {}",
            path.display()
        );
        return Ok(());
    }
    let mut file = NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    if immutable {
        file.persist_noclobber(path)?;
    } else {
        file.persist(path)?;
    }
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn store(repo: &Path, kind: &str, bytes: &[u8]) -> Result<String> {
    let hash = digest(bytes);
    write_atomic(&repo.join(kind).join(&hash), bytes, true)?;
    Ok(hash)
}

pub fn publish(repo: &Path, proto: &Path, recipe: &Path, key: &Path) -> Result<String> {
    let key = Ed25519KeyPair::from_pkcs8_maybe_unchecked(&fs::read(key)?)
        .map_err(|e| anyhow::anyhow!("invalid Ed25519 PKCS#8 signing key: {e}"))?;
    let mut manifest: Manifest = toml::from_str(&fs::read_to_string(recipe)?)
        .with_context(|| format!("parsing {}", recipe.display()))?;
    manifest.validate(false)?;
    let proto = Root::open(proto)?;
    fs::create_dir_all(repo)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(repo.join("publish.lock"))?;
    flock(&lock, FlockOperation::LockExclusive)?;
    for dir in ["catalogs", "manifests", "blobs"] {
        fs::create_dir_all(repo.join(dir))?;
    }
    let mut catalog = if repo.join("current").try_exists()? {
        read_catalog(
            repo,
            &read_current(repo)?,
            &manifest.publisher,
            key.public_key().as_ref(),
        )?
    } else {
        Catalog {
            format: FORMAT,
            publisher: manifest.publisher.clone(),
            entries: BTreeMap::new(),
        }
    };
    for action in &mut manifest.action {
        if let Action::File {
            path, sha256, size, ..
        } = action
        {
            let bytes = proto
                .read_file(path)
                .with_context(|| format!("reading build output {path}"))?;
            *sha256 = Some(store(repo, "blobs", &bytes)?);
            *size = Some(bytes.len() as u64);
        }
    }
    manifest.validate(true)?;
    let id = manifest.id();
    let sha256 = store(repo, "manifests", &serde_json::to_vec(&manifest)?)?;
    if let Some(existing) = catalog.entries.get(&id) {
        ensure!(
            *existing == sha256,
            "{id} was already published with different content"
        );
        return Ok(id);
    }
    catalog.entries.insert(id.clone(), sha256);
    let signature = hex::encode(key.sign(&serde_json::to_vec(&catalog)?).as_ref());
    let signed = SignedCatalog { catalog, signature };
    let hash = store(repo, "catalogs", &serde_json::to_vec(&signed)?)?;
    File::open(repo)?.sync_all()?;
    write_atomic(&repo.join("current"), format!("{hash}\n").as_bytes(), false)?;
    Ok(id)
}

pub fn read_blob(repo: &Path, node: &Node) -> Result<Vec<u8>> {
    let Node::File { sha256, size, .. } = node else {
        anyhow::bail!("expected a file payload");
    };
    let bytes = read_object(repo, "blobs", sha256)?;
    ensure!(
        bytes.len() as u64 == *size,
        "payload length mismatch for {sha256}"
    );
    Ok(bytes)
}

pub fn canonical_repo(repo: &Path) -> Result<PathBuf> {
    fs::canonicalize(repo).with_context(|| format!("opening repository {}", repo.display()))
}
