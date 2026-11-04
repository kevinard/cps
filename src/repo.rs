use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::{
    attestation::{ATTESTATION_LIMIT, VerifiedBuild, verify_bundle},
    image::root::Root,
    kdl_format::{manifest_text, parse_manifest},
    model::{Action, FORMAT, FileDigest, Manifest, Node, digest, digest_reader, validate_digest},
    systemd::{self, Action as SystemdAction},
};

const CATALOG_LIMIT: u64 = 64 * 1024 * 1024;
pub(crate) const MANIFEST_LIMIT: u64 = 32 * 1024 * 1024;
const TMPFILES_LIMIT: u64 = 1024 * 1024;

pub mod release;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    format: u32,
    publisher: String,
    entries: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    attestations: BTreeMap<String, String>,
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
    let bytes = read_object(repo, "manifests", &sha256, MANIFEST_LIMIT)?;
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
    attestations: BTreeMap<String, String>,
}

pub fn read_key(path: &Path) -> Result<Vec<u8>> {
    let bytes = (|| read_limited(File::open(path)?, 44))()
        .with_context(|| format!("reading trust key {}", path.display()))?;
    // SubjectPublicKeyInfo for Ed25519, as emitted by openssl pkey -pubout -outform DER.
    const PREFIX: &[u8] = b"\x30\x2a\x30\x05\x06\x03\x2b\x65\x70\x03\x21\x00";
    let key = bytes
        .strip_prefix(PREFIX)
        .context("expected an Ed25519 DER public key")?;
    ensure!(key.len() == 32, "invalid Ed25519 public key length");
    Ok(key.to_vec())
}

pub(crate) fn read_signing_key(path: &Path) -> Result<Ed25519KeyPair> {
    let bytes = read_limited(File::open(path)?, 1024)
        .with_context(|| format!("reading signing key {}", path.display()))?;
    Ed25519KeyPair::from_pkcs8_maybe_unchecked(&bytes)
        .map_err(|e| anyhow::anyhow!("invalid Ed25519 PKCS#8 signing key: {e}"))
}

fn read_current(repo: &Path) -> Result<String> {
    let pointer = String::from_utf8(
        read_limited(Root::open(repo)?.file("current")?, 65)
            .context("reading repository current catalog")?,
    )?;
    parse_current(&pointer)
}

fn parse_current(pointer: &str) -> Result<String> {
    let hash = pointer.trim_end();
    validate_digest(hash)?;
    Ok(hash.to_owned())
}

pub(crate) fn read_limited(file: File, limit: u64) -> Result<Vec<u8>> {
    ensure!(
        file.metadata()?.len() <= limit,
        "repository file exceeds {limit} bytes"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "repository file exceeds {limit} bytes"
    );
    Ok(bytes)
}

fn read_object(repo: &Path, kind: &str, hash: &str, limit: u64) -> Result<Vec<u8>> {
    validate_digest(hash)?;
    let object = format!("{kind}/{hash}");
    let bytes = read_limited(Root::open(repo)?.file(&object)?, limit)
        .with_context(|| format!("reading repository object {object}"))?;
    ensure!(
        digest(&bytes) == hash,
        "digest mismatch for repository object {object}"
    );
    Ok(bytes)
}

fn read_catalog(repo: &Path, hash: &str, publisher: &str, key: &[u8]) -> Result<Catalog> {
    let signed: SignedCatalog =
        serde_json::from_slice(&read_object(repo, "catalogs", hash, CATALOG_LIMIT)?)?;
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
    let mut evidence = BTreeSet::new();
    for (id, digest) in &catalog.attestations {
        validate_digest(digest)?;
        ensure!(
            catalog.entries.contains_key(id),
            "attestation for absent package {id}"
        );
        ensure!(
            evidence.insert(digest),
            "duplicate package attestation {digest}"
        );
    }
    Ok(catalog)
}

pub fn read_attestation(repo: &Path, publisher: &str, key: &Path, id: &str) -> Result<Vec<u8>> {
    let catalog = read_catalog(repo, &read_current(repo)?, publisher, &read_key(key)?)?;
    let hash = catalog
        .attestations
        .get(id)
        .with_context(|| format!("no build attestation for {id}"))?;
    read_object(repo, "attestations", hash, ATTESTATION_LIMIT)
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
        attestations: catalog.attestations,
    })
}

fn write_atomic(path: &Path, bytes: &[u8], immutable: bool) -> Result<()> {
    let parent = path.parent().context("missing object parent")?;
    if immutable && path.try_exists()? {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("invalid object name")?;
        ensure!(
            read_limited(Root::open(parent)?.file(name)?, bytes.len() as u64)? == bytes,
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

fn lock_repo(repo: &Path) -> Result<File> {
    fs::create_dir_all(repo)?;
    Root::open(repo)?;
    let lock = File::from(open(
        repo.join("publish.lock"),
        OFlags::CREATE | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    ensure!(lock.metadata()?.is_file(), "invalid repository lock");
    flock(&lock, FlockOperation::LockExclusive)?;
    Ok(lock)
}

fn path_present(path: &Path) -> Result<bool> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn prepare_objects(repo: &Path) -> Result<()> {
    let root = Root::open(repo)?;
    for dir in ["catalogs", "manifests", "blobs", "attestations"] {
        if !path_present(&repo.join(dir))? {
            fs::create_dir(repo.join(dir))?;
        }
        root.dir(dir)?;
    }
    File::open(repo)?.sync_all()?;
    Ok(())
}

pub fn publish(repo: &Path, proto: &Path, recipe: &Path, key: &Path) -> Result<String> {
    let key = read_signing_key(key)?;
    let manifest_bytes = read_limited(File::open(recipe)?, MANIFEST_LIMIT)?;
    let manifest = parse_manifest(std::str::from_utf8(&manifest_bytes)?)
        .with_context(|| format!("parsing {}", recipe.display()))?;
    manifest.validate(false)?;
    publish_manifest(repo, Root::open(proto)?, manifest, &key, None)
}

pub fn publish_bundle(
    repo: &Path,
    bundle: &Path,
    key: &Path,
    builder_key: &Path,
    reviewed_recipe: &str,
) -> Result<String> {
    let bundle = Root::open(bundle)?;
    let key = read_signing_key(key)?;
    let builder = read_key(builder_key)?;
    ensure!(
        builder.as_slice() != key.public_key().as_ref(),
        "builder and publisher must use distinct keys"
    );
    let (manifest, verified) = verify_bundle(&bundle, &builder, reviewed_recipe)?;
    publish_manifest(
        repo,
        bundle.subdir("proto")?,
        manifest,
        &key,
        Some(verified),
    )
}

fn publish_manifest(
    repo: &Path,
    proto: Root,
    mut manifest: Manifest,
    key: &Ed25519KeyPair,
    evidence: Option<VerifiedBuild>,
) -> Result<String> {
    let _lock = lock_repo(repo)?;
    ensure!(
        !path_present(&repo.join(".tuf"))?,
        "cannot publish to a TUF-managed repository cache"
    );
    prepare_objects(repo)?;
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
            attestations: BTreeMap::new(),
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
            let input = proto
                .file(path)
                .with_context(|| format!("reading build output {path}"))?;
            let expected = evidence
                .as_ref()
                .map(|build| {
                    build
                        .artifacts
                        .get(path)
                        .context("missing verified build output")
                })
                .transpose()?;
            let FileDigest {
                sha256: hash,
                size: length,
            } = store_blob(repo, input, tmpfiles.contains(path), expected)
                .with_context(|| format!("publishing {path}"))?;
            *sha256 = Some(hash);
            *size = Some(length);
        }
    }
    manifest.validate(true)?;
    let id = manifest.id();
    let unchanged_manifest = if let Some(existing) = catalog.entries.get(&id) {
        let previous = load_manifest(repo, existing.clone())?;
        ensure!(
            previous.manifest == manifest,
            "{id} was already published with different content"
        );
        true
    } else {
        let manifest = manifest_text(&manifest);
        ensure!(
            manifest.len() as u64 <= MANIFEST_LIMIT,
            "manifest exceeds {MANIFEST_LIMIT} bytes"
        );
        let sha256 = store(repo, "manifests", manifest.as_bytes())?;
        catalog.entries.insert(id.clone(), sha256);
        false
    };
    if let Some(evidence) = evidence {
        let hash = digest(&evidence.envelope);
        if let Some(previous) = catalog.attestations.get(&id) {
            ensure!(
                previous == &hash,
                "{id} already has a different build attestation"
            );
            read_object(repo, "attestations", previous, ATTESTATION_LIMIT)?;
            return Ok(id);
        }
        let hash = store(repo, "attestations", &evidence.envelope)?;
        catalog.attestations.insert(id.clone(), hash);
    } else if unchanged_manifest {
        return Ok(id);
    }
    let signature = hex::encode(key.sign(&serde_json::to_vec(&catalog)?).as_ref());
    let signed = SignedCatalog { catalog, signature };
    let signed = serde_json::to_vec(&signed)?;
    ensure!(
        signed.len() as u64 <= CATALOG_LIMIT,
        "catalog exceeds {CATALOG_LIMIT} bytes"
    );
    let hash = store(repo, "catalogs", &signed)?;
    File::open(repo)?.sync_all()?;
    write_atomic(&repo.join("current"), format!("{hash}\n").as_bytes(), false)?;
    Ok(id)
}

fn store_blob(
    repo: &Path,
    mut input: File,
    validate_tmpfiles: bool,
    expected: Option<&FileDigest>,
) -> Result<FileDigest> {
    let parent = repo.join("blobs");
    let mut temp = NamedTempFile::new_in(&parent)?;
    let size = if let Some(file) = expected {
        ensure!(
            input.metadata()?.len() == file.size,
            "attested payload length mismatch"
        );
        let size = io::copy(&mut input.take(file.size.saturating_add(1)), &mut temp)?;
        ensure!(size == file.size, "attested payload length mismatch");
        size
    } else {
        io::copy(&mut input, &mut temp)?
    };
    temp.as_file_mut().rewind()?;
    if validate_tmpfiles {
        let bytes = read_limited(temp.reopen()?, TMPFILES_LIMIT)?;
        systemd::validate_tmpfiles(&bytes)?;
    }
    let hash = digest_reader(temp.as_file_mut())?;
    ensure!(
        expected.is_none_or(|file| file.sha256 == hash),
        "attested payload digest mismatch"
    );
    let path = parent.join(&hash);
    if path.try_exists()? {
        let existing = Root::open(repo)?.file(&format!("blobs/{hash}"))?;
        ensure!(
            existing.metadata()?.len() == size
                && digest_reader(existing.take(size.saturating_add(1)))? == hash,
            "immutable payload differs at {}",
            path.display()
        );
    } else {
        temp.as_file().sync_all()?;
        temp.persist_noclobber(path)?;
        File::open(parent)?.sync_all()?;
    }
    Ok(FileDigest { sha256: hash, size })
}

pub fn stage_blob(repo: &Path, node: &Node, destination: &mut File) -> Result<()> {
    let Node::File { sha256, size, .. } = node else {
        anyhow::bail!("expected a file payload");
    };
    validate_digest(sha256)?;
    let source = Root::open(repo)?
        .file(&format!("blobs/{sha256}"))
        .with_context(|| format!("opening repository payload {sha256}"))?;
    ensure!(
        source.metadata()?.len() == *size,
        "payload length mismatch for {sha256}"
    );
    let copied = io::copy(&mut source.take(size.saturating_add(1)), destination)?;
    ensure!(copied == *size, "payload length mismatch for {sha256}");
    destination.rewind()?;
    ensure!(
        digest_reader(destination)? == *sha256,
        "payload digest mismatch for {sha256}"
    );
    Ok(())
}

pub fn canonical_repo(repo: &Path) -> Result<PathBuf> {
    fs::canonicalize(repo).with_context(|| format!("opening repository {}", repo.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_and_payload_reads_are_bounded() -> Result<()> {
        let repo = tempfile::tempdir()?;
        let path = repo.path();
        fs::create_dir(path.join("catalogs"))?;
        fs::create_dir(path.join("blobs"))?;
        let hash = digest(b"abc");
        File::create(path.join("catalogs").join(&hash))?.set_len(CATALOG_LIMIT + 1)?;
        let error = read_object(path, "catalogs", &hash, CATALOG_LIMIT).unwrap_err();
        assert!(format!("{error:#}").contains("exceeds"));
        fs::write(path.join("current"), vec![b'a'; 66])?;
        assert!(read_current(path).is_err());

        let blob = path.join("blobs").join(&hash);
        let node = Node::File {
            sha256: hash,
            size: 3,
            mode: 0o644,
            uid: 0,
            gid: 0,
        };
        fs::write(&blob, b"abcd")?;
        let mut destination = tempfile::tempfile()?;
        assert!(
            stage_blob(path, &node, &mut destination)
                .unwrap_err()
                .to_string()
                .contains("length mismatch")
        );
        assert_eq!(destination.metadata()?.len(), 0);
        fs::write(&blob, b"xyz")?;
        assert!(
            stage_blob(path, &node, &mut destination)
                .unwrap_err()
                .to_string()
                .contains("digest mismatch")
        );
        destination.set_len(0)?;
        destination.rewind()?;
        fs::write(&blob, b"abc")?;
        stage_blob(path, &node, &mut destination)?;
        destination.rewind()?;
        assert_eq!(digest_reader(destination)?, digest(b"abc"));
        Ok(())
    }

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
