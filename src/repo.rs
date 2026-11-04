use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Seek, Write},
    num::NonZeroU64,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::{
    attestation::{ATTESTATION_LIMIT, MAX_BUILDERS, VerifiedBuild, verify_bundle},
    image::root::Root,
    kdl_format::{manifest_text, parse_manifest, publish_policy, publish_release},
    model::{
        Action, FORMAT, FileDigest, Manifest, Node, PackageSpec, digest, digest_reader,
        validate_digest,
    },
    systemd::{self, Action as SystemdAction},
};

const CATALOG_LIMIT: u64 = 64 * 1024 * 1024;
pub(crate) const MANIFEST_LIMIT: u64 = 32 * 1024 * 1024;
const TMPFILES_LIMIT: u64 = 1024 * 1024;
const PUBLISH_POLICY_LIMIT: u64 = 1024 * 1024;
const PUBLISH_RELEASE_LIMIT: u64 = 1024 * 1024;
const PUBLISH_POLICY: &str = "publish-policy.kdl";

pub mod release;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    format: u32,
    publisher: String,
    entries: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    attestations: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    attestations_required: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    withdrawals: BTreeMap<String, Withdrawal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sequence: Option<NonZeroU64>,
}

impl Catalog {
    fn empty(publisher: &str) -> Self {
        Self {
            format: FORMAT,
            publisher: publisher.to_owned(),
            entries: BTreeMap::new(),
            attestations: BTreeMap::new(),
            attestations_required: false,
            withdrawals: BTreeMap::new(),
            sequence: None,
        }
    }

    fn withdraw(&mut self, id: &str) -> Result<bool> {
        if self.withdrawals.contains_key(id) {
            return Ok(false);
        }
        let manifest = self
            .entries
            .remove(id)
            .with_context(|| format!("no published package {id}"))?;
        let attestation = self.attestations.remove(id);
        self.withdrawals.insert(
            id.to_owned(),
            Withdrawal {
                manifest,
                attestation,
            },
        );
        Ok(true)
    }

    fn check_successor(&self, next: &Self) -> Result<()> {
        if let Some(previous) = self.sequence {
            ensure!(
                next.sequence.is_some_and(|sequence| sequence > previous),
                "publisher catalog rollback or fork after sequence {previous}"
            );
        }
        ensure!(
            !self.attestations_required || next.attestations_required,
            "release disables the publisher's build evidence requirement"
        );
        for (id, record) in &self.withdrawals {
            ensure!(
                next.withdrawals.get(id) == Some(record),
                "release drops or changes the withdrawal for {id}"
            );
        }
        for (id, manifest) in &self.entries {
            let (next_manifest, next_attestation) = match next.withdrawals.get(id) {
                Some(record) => (Some(&record.manifest), record.attestation.as_ref()),
                None => (next.entries.get(id), next.attestations.get(id)),
            };
            ensure!(
                next_manifest == Some(manifest),
                "release drops or changes the published manifest for {id}"
            );
            if let Some(attestation) = self.attestations.get(id) {
                ensure!(
                    next_attestation == Some(attestation),
                    "release drops or changes the build attestation for {id}"
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Withdrawal {
    manifest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attestation: Option<String>,
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
    withdrawals: BTreeMap<String, Withdrawal>,
}

impl Snapshot {
    pub(crate) fn is_withdrawn(&self, id: &str) -> bool {
        self.withdrawals.contains_key(id)
    }
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
    read_pointer(repo, "current")
}

fn read_pointer(repo: &Path, name: &str) -> Result<String> {
    let pointer = String::from_utf8(
        read_limited(Root::open(repo)?.file(name)?, 65)
            .with_context(|| format!("reading repository pointer {name}"))?,
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
    for (id, manifest) in &catalog.entries {
        ensure!(
            exact_publisher(id)? == publisher,
            "invalid catalog entry {id}"
        );
        validate_digest(manifest)?;
    }
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
    ensure!(
        !catalog.attestations_required || catalog.attestations.len() == catalog.entries.len(),
        "protected catalog contains a package without build evidence"
    );
    for (id, withdrawal) in &catalog.withdrawals {
        ensure!(
            exact_publisher(id)? == publisher && !catalog.entries.contains_key(id),
            "invalid withdrawal for {id}"
        );
        validate_digest(&withdrawal.manifest)?;
        if let Some(digest) = &withdrawal.attestation {
            validate_digest(digest)?;
            ensure!(
                evidence.insert(digest),
                "duplicate package attestation {digest}"
            );
        }
    }
    Ok(catalog)
}

pub fn read_attestation(repo: &Path, publisher: &str, key: &Path, id: &str) -> Result<Vec<u8>> {
    let catalog = read_catalog(repo, &read_current(repo)?, publisher, &read_key(key)?)?;
    let hash = catalog
        .attestations
        .get(id)
        .or_else(|| catalog.withdrawals.get(id)?.attestation.as_ref())
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
        packages.push(read_package(repo, publisher, &id, sha256)?);
    }
    Ok(Snapshot {
        sha256: hash,
        packages,
        attestations: catalog.attestations,
        withdrawals: catalog.withdrawals,
    })
}

pub(crate) fn catalog_entries(
    repo: &Path,
    publisher: &str,
    key: &[u8],
) -> Result<BTreeMap<String, String>> {
    Ok(read_catalog(repo, &read_current(repo)?, publisher, key)?.entries)
}

pub(crate) fn read_package(
    repo: &Path,
    publisher: &str,
    id: &str,
    sha256: String,
) -> Result<Published> {
    let package = load_manifest(repo, sha256)?;
    ensure!(
        package.manifest.id() == id && package.manifest.publisher == publisher,
        "manifest identity mismatch for {id}"
    );
    Ok(package)
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

fn exact_publisher(id: &str) -> Result<&str> {
    let spec = PackageSpec::parse(id)?;
    spec.publisher
        .filter(|_| spec.version.is_some())
        .context("expected a full package ID with publisher and version")
}

pub fn withdraw_package(repo: &Path, key: &Path, id: &str) -> Result<()> {
    let publisher = exact_publisher(id)?;
    let key = read_signing_key(key)?;
    Root::open(repo)?;
    let _lock = lock_repo(repo)?;
    ensure!(
        !path_present(&repo.join(".tuf"))?,
        "cannot withdraw from a TUF-managed repository cache"
    );
    let mut catalog = read_catalog(
        repo,
        &read_current(repo)?,
        publisher,
        key.public_key().as_ref(),
    )?;
    if catalog.withdraw(id)? {
        write_catalog(repo, catalog, &key)?;
    }
    Ok(())
}

pub fn list_withdrawals(
    repo: &Path,
    publisher: &str,
    key: &Path,
) -> Result<BTreeMap<String, String>> {
    let catalog = read_catalog(repo, &read_current(repo)?, publisher, &read_key(key)?)?;
    Ok(catalog
        .withdrawals
        .into_iter()
        .map(|(id, withdrawal)| (id, withdrawal.manifest))
        .collect())
}

pub fn publish_bundle(
    repo: &Path,
    bundle: &Path,
    key: &Path,
    builder_key: &Path,
    reviewed_recipe: &str,
) -> Result<String> {
    publish_witnessed_bundle(repo, bundle, key, builder_key, reviewed_recipe, &[])
}

#[derive(Clone)]
pub struct BuildWitness {
    pub key: PathBuf,
    pub attestation: PathBuf,
}

pub fn publish_witnessed_bundle(
    repo: &Path,
    bundle: &Path,
    key: &Path,
    builder_key: &Path,
    reviewed_recipe: &str,
    witnesses: &[BuildWitness],
) -> Result<String> {
    let key = read_signing_key(key)?;
    let (proto, manifest, verified) = verify_publication(
        bundle,
        builder_key,
        reviewed_recipe,
        witnesses,
        key.public_key().as_ref(),
    )?;
    publish_manifest(repo, proto, manifest, &key, Some(verified))
}

fn verify_publication(
    bundle: &Path,
    builder_key: &Path,
    reviewed_recipe: &str,
    witnesses: &[BuildWitness],
    publisher_key: &[u8],
) -> Result<(Root, Manifest, VerifiedBuild)> {
    ensure!(
        witnesses.len() < MAX_BUILDERS,
        "at most {MAX_BUILDERS} build signers are supported"
    );
    let bundle = Root::open(bundle)?;
    let builder = read_key(builder_key)?;
    ensure!(
        builder.as_slice() != publisher_key,
        "builder and publisher must use distinct keys"
    );
    let (manifest, mut verified) = verify_bundle(&bundle, &builder, reviewed_recipe)?;
    for witness in witnesses {
        let builder = read_key(&witness.key)?;
        ensure!(
            builder.as_slice() != publisher_key,
            "builder and publisher must use distinct keys"
        );
        let file = File::from(open(
            &witness.attestation,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        ensure!(
            file.metadata()?.is_file(),
            "witness attestation must be a regular file"
        );
        let envelope = read_limited(file, ATTESTATION_LIMIT)?;
        verified
            .add_witness(&envelope, &builder)
            .with_context(|| format!("verifying witness {}", witness.attestation.display()))?;
    }
    Ok((bundle.subdir("proto")?, manifest, verified))
}

pub fn publish_release(repo: &Path, key: &Path, document: &Path) -> Result<String> {
    let bytes = read_limited(File::open(document)?, PUBLISH_RELEASE_LIMIT)?;
    let release = publish_release::parse(std::str::from_utf8(&bytes)?)
        .with_context(|| format!("reading release {}", document.display()))?;
    let parent = document.parent().unwrap_or_else(|| Path::new("."));
    let key = read_signing_key(key)?;
    let _lock = lock_repo(repo)?;
    let current = publication_base(repo)?;
    ensure!(
        release.base == current,
        "release base is stale: expected {}, current is {}",
        release.base.as_deref().unwrap_or("empty"),
        current.as_deref().unwrap_or("empty"),
    );
    let mut catalog = publication_catalog(repo, &release.publisher, &key, current.as_deref())?;
    let policy = if release
        .changes
        .iter()
        .any(|change| matches!(change, publish_release::Change::Bundle { .. }))
    {
        prepare_objects(repo)?;
        read_publish_policy(repo, &catalog)?
    } else {
        None
    };
    let mut changed = false;
    for change in release.changes {
        match change {
            publish_release::Change::Bundle {
                id,
                path,
                builder_key,
                recipe,
                mut witnesses,
            } => {
                for witness in &mut witnesses {
                    witness.key = parent.join(&witness.key);
                    witness.attestation = parent.join(&witness.attestation);
                }
                let (proto, manifest, verified) = verify_publication(
                    &parent.join(path),
                    &parent.join(builder_key),
                    &recipe,
                    &witnesses,
                    key.public_key().as_ref(),
                )
                .with_context(|| format!("verifying bundle for {id}"))?;
                ensure!(
                    manifest.id() == id,
                    "bundle contains {} instead of {id}",
                    manifest.id()
                );
                changed |= stage_manifest(
                    repo,
                    &mut catalog,
                    proto,
                    manifest,
                    Some(verified),
                    policy.as_ref(),
                )
                .with_context(|| format!("staging {id}"))?;
            }
            publish_release::Change::Withdraw(id) => changed |= catalog.withdraw(&id)?,
        }
    }
    if changed {
        write_catalog(repo, catalog, &key)
    } else {
        current.context("empty release has no published catalog")
    }
}

fn publication_base(repo: &Path) -> Result<Option<String>> {
    ensure!(
        !path_present(&repo.join(".tuf"))?,
        "cannot publish to a TUF-managed repository cache"
    );
    path_present(&repo.join("current"))?
        .then(|| read_current(repo))
        .transpose()
}

fn publication_catalog(
    repo: &Path,
    publisher: &str,
    key: &Ed25519KeyPair,
    current: Option<&str>,
) -> Result<Catalog> {
    match current {
        Some(hash) => read_catalog(repo, hash, publisher, key.public_key().as_ref()),
        None => Ok(Catalog::empty(publisher)),
    }
}

fn publish_manifest(
    repo: &Path,
    proto: Root,
    manifest: Manifest,
    key: &Ed25519KeyPair,
    evidence: Option<VerifiedBuild>,
) -> Result<String> {
    let _lock = lock_repo(repo)?;
    let current = publication_base(repo)?;
    let mut catalog = publication_catalog(repo, &manifest.publisher, key, current.as_deref())?;
    let id = manifest.id();
    let policy = read_publish_policy(repo, &catalog)?;
    prepare_objects(repo)?;
    if stage_manifest(
        repo,
        &mut catalog,
        proto,
        manifest,
        evidence,
        policy.as_ref(),
    )? {
        write_catalog(repo, catalog, key)?;
    }
    Ok(id)
}

fn stage_manifest(
    repo: &Path,
    catalog: &mut Catalog,
    proto: Root,
    mut manifest: Manifest,
    evidence: Option<VerifiedBuild>,
    policy: Option<&PublicationPolicy>,
) -> Result<bool> {
    ensure!(
        manifest.publisher == catalog.publisher,
        "bundle belongs to a different publisher"
    );
    let id = manifest.id();
    ensure!(
        !catalog.withdrawals.contains_key(&id),
        "{id} was withdrawn and cannot be republished"
    );
    if let Some(policy) = policy {
        policy.verify(&id, evidence.as_ref())?;
        catalog.attestations_required = true;
    }
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
        let envelope = evidence.envelope()?;
        let hash = digest(&envelope);
        if let Some(previous) = catalog.attestations.get(&id) {
            ensure!(
                previous == &hash,
                "{id} already has a different build attestation"
            );
            read_object(repo, "attestations", previous, ATTESTATION_LIMIT)?;
            return Ok(false);
        }
        let hash = store(repo, "attestations", &envelope)?;
        catalog.attestations.insert(id.clone(), hash);
    } else if unchanged_manifest {
        return Ok(false);
    }
    Ok(true)
}

fn write_catalog(repo: &Path, mut catalog: Catalog, key: &Ed25519KeyPair) -> Result<String> {
    let sequence = catalog
        .sequence
        .map_or(0, NonZeroU64::get)
        .checked_add(1)
        .and_then(NonZeroU64::new)
        .context("publisher catalog sequence exhausted")?;
    catalog.sequence = Some(sequence);
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
    Ok(hash)
}

struct PublicationPolicy {
    approved: BTreeSet<String>,
    recipes: BTreeMap<String, String>,
    minimum_builders: usize,
}

impl PublicationPolicy {
    fn verify(&self, id: &str, evidence: Option<&VerifiedBuild>) -> Result<()> {
        let build = evidence.context("publish policy requires an attested build bundle")?;
        ensure!(
            self.recipes.get(id) == Some(&build.recipe_sha256),
            "build recipe is not approved for {id}"
        );
        ensure!(
            build
                .builders()
                .all(|builder| self.approved.contains(builder)),
            "build signer is not authorized by publish policy"
        );
        ensure!(
            build.builders().len() >= self.minimum_builders,
            "publish policy requires {} distinct build signers",
            self.minimum_builders
        );
        Ok(())
    }
}

fn read_publish_policy(repo: &Path, catalog: &Catalog) -> Result<Option<PublicationPolicy>> {
    if !path_present(&repo.join(PUBLISH_POLICY))? {
        ensure!(
            !catalog.attestations_required,
            "protected repository is missing {PUBLISH_POLICY}"
        );
        return Ok(None);
    }
    ensure!(
        catalog.attestations_required || catalog.entries.is_empty(),
        "configure publish policy before the repository's first publication"
    );
    let bytes = read_limited(
        Root::open(repo)?.file(PUBLISH_POLICY)?,
        PUBLISH_POLICY_LIMIT,
    )?;
    let policy = publish_policy::parse(std::str::from_utf8(&bytes)?)
        .with_context(|| format!("reading {PUBLISH_POLICY}"))?;
    ensure!(
        policy.publisher == catalog.publisher,
        "publish policy names a different publisher"
    );
    let mut approved = BTreeSet::new();
    for path in policy.builder_keys {
        let key = read_key(&repo.join(path))?;
        approved.insert(digest(&key));
    }
    ensure!(
        approved.len() >= policy.minimum_builders,
        "publish policy has fewer distinct builder keys than its threshold"
    );
    Ok(Some(PublicationPolicy {
        approved,
        recipes: policy.recipes,
        minimum_builders: policy.minimum_builders,
    }))
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
    fn legacy_catalog_signatures_and_first_sequence() -> Result<()> {
        let repo = tempfile::tempdir()?;
        prepare_objects(repo.path())?;
        let key =
            Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).expect("generate key");
        let key = Ed25519KeyPair::from_pkcs8(key.as_ref()).expect("read generated key");
        let legacy = br#"{"format":1,"publisher":"canopa","entries":{}}"#;
        let signed = format!(
            r#"{{"catalog":{},"signature":"{}"}}"#,
            std::str::from_utf8(legacy)?,
            hex::encode(key.sign(legacy).as_ref())
        );
        let hash = store(repo.path(), "catalogs", signed.as_bytes())?;
        let read =
            |hash: &str| read_catalog(repo.path(), hash, "canopa", key.public_key().as_ref());
        let previous = read(&hash)?;
        assert!(previous.sequence.is_none());
        write_catalog(repo.path(), previous.clone(), &key)?;
        let next = read(&read_current(repo.path())?)?;
        assert_eq!(next.sequence, NonZeroU64::new(1));
        previous.check_successor(&next)?;
        assert!(next.check_successor(&previous).is_err());
        Ok(())
    }

    #[test]
    fn catalog_successors_preserve_signed_publication_records() -> Result<()> {
        let active = "cps://canopa/desktop@2.0.0-0".to_owned();
        let withdrawn = "cps://canopa/desktop@1.0.0-0".to_owned();
        let manifest = digest(b"active manifest");
        let attestation = digest(b"build evidence");
        let previous = Catalog {
            format: FORMAT,
            publisher: "canopa".to_owned(),
            entries: BTreeMap::from([(active.clone(), manifest.clone())]),
            attestations: BTreeMap::from([(active.clone(), attestation.clone())]),
            attestations_required: true,
            withdrawals: BTreeMap::from([(
                withdrawn.clone(),
                Withdrawal {
                    manifest: digest(b"withdrawn manifest"),
                    attestation: None,
                },
            )]),
            sequence: NonZeroU64::new(1),
        };
        let mut next = previous.clone();
        next.sequence = NonZeroU64::new(3);
        previous.check_successor(&next)?;

        next.attestations_required = false;
        assert!(previous.check_successor(&next).is_err());
        next.attestations_required = true;

        next.attestations.remove(&active);
        assert!(previous.check_successor(&next).is_err());
        next.entries.remove(&active);
        assert!(previous.check_successor(&next).is_err());
        next.withdrawals.insert(
            active,
            Withdrawal {
                manifest,
                attestation: Some(attestation),
            },
        );
        previous.check_successor(&next)?;

        next.withdrawals.remove(&withdrawn);
        assert!(previous.check_successor(&next).is_err());
        Ok(())
    }

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
            capabilities: None,
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
        let bytes = br#"{"format":1,"publisher":"canopa","name":"test/legacy","version":"1.0.0-0","dependency":[{"kind":"require","package":"library/one","version":null},{"kind":"incorporate","package":"library/two","version":"=1.0.0-0"}],"action":[]}"#;
        let hash = store(repo.path(), "manifests", bytes)?;
        let manifest = load_manifest(repo.path(), hash.clone())?;
        assert_eq!(manifest.sha256, hash);
        assert!(manifest.document.is_none());
        // The old installed record has no KDL document and verifies offline.
        let manifest: Published = serde_json::from_slice(&serde_json::to_vec(&manifest)?)?;
        manifest.verify()
    }
}
