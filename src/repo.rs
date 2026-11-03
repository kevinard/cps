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
    kdl_format::{manifest_text, parse_manifest},
    model::{Action, FORMAT, Manifest, Node, digest, validate_digest},
    systemd::{self, Action as SystemdAction},
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    document: Option<String>,
}

impl Published {
    pub fn verify(&self) -> Result<()> {
        self.manifest.validate(true)?;
        let bytes = if let Some(document) = &self.document {
            ensure!(
                parse_manifest(document)? == self.manifest,
                "stored manifest does not match its KDL document"
            );
            document.as_bytes().to_vec()
        } else {
            // Existing images and catalogs contain canonical JSON manifests.
            serde_json::to_vec(&self.manifest)?
        };
        ensure!(
            digest(&bytes) == self.sha256,
            "installed manifest digest mismatch for {}",
            self.manifest.id()
        );
        Ok(())
    }
}

fn load_manifest(repo: &Path, sha256: String) -> Result<Published> {
    let bytes = read_object(repo, "manifests", &sha256)?;
    let (manifest, document) = if bytes.starts_with(b"{") {
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        ensure!(
            serde_json::to_vec(&manifest)? == bytes,
            "noncanonical legacy manifest {sha256}"
        );
        (manifest, None)
    } else {
        let document = String::from_utf8(bytes)?;
        (parse_manifest(&document)?, Some(document))
    };
    manifest.validate(true)?;
    Ok(Published {
        sha256,
        manifest,
        document,
    })
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
        let package = load_manifest(repo, sha256)?;
        ensure!(
            package.manifest.id() == id && package.manifest.publisher == publisher,
            "manifest identity mismatch for {id}"
        );
        packages.push(package);
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
    let mut manifest = parse_manifest(&fs::read_to_string(recipe)?)
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
    let tmpfiles: std::collections::BTreeSet<_> = manifest
        .systemd
        .iter()
        .filter(|effect| matches!(effect, SystemdAction::Tmpfiles(_)))
        .map(SystemdAction::path)
        .collect();
    for action in &mut manifest.action {
        if let Action::File {
            path, sha256, size, ..
        } = action
        {
            let bytes = proto
                .read_file(path)
                .with_context(|| format!("reading build output {path}"))?;
            if tmpfiles.contains(path) {
                systemd::validate_tmpfiles(&bytes).with_context(|| format!("validating {path}"))?;
            }
            *sha256 = Some(store(repo, "blobs", &bytes)?);
            *size = Some(bytes.len() as u64);
        }
    }
    manifest.validate(true)?;
    let id = manifest.id();
    if let Some(existing) = catalog.entries.get(&id) {
        let previous = load_manifest(repo, existing.clone())?;
        ensure!(
            previous.manifest == manifest,
            "{id} was already published with different content"
        );
        return Ok(id);
    }
    let sha256 = store(repo, "manifests", manifest_text(&manifest).as_bytes())?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_json_manifest_keeps_its_digest() -> Result<()> {
        let repo = tempfile::tempdir()?;
        fs::create_dir(repo.path().join("manifests"))?;
        let bytes = br#"{"format":1,"publisher":"canopa","name":"test/legacy","version":"1.0.0-0","dependency":[],"action":[]}"#;
        let hash = store(repo.path(), "manifests", bytes)?;
        let manifest = load_manifest(repo.path(), hash.clone())?;
        assert_eq!(manifest.sha256, hash);
        assert!(manifest.document.is_none());
        // The old installed record has no KDL document and verifies offline.
        let manifest: Published = serde_json::from_slice(&serde_json::to_vec(&manifest)?)?;
        manifest.verify()
    }
}
