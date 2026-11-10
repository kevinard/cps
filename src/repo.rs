use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Seek, Write},
    num::NonZeroU64,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use anyhow::{Context, Result, ensure};
use ring::signature::Ed25519KeyPair;
use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::{
    attestation::{ATTESTATION_LIMIT, BuildTrust, MAX_BUILDERS, VerifiedBuild, verify_bundle},
    image::root::Root,
    kdl_format::{manifest_text, parse_manifest, publish_policy, publish_release},
    model::{
        Dependency, FileDigest, Manifest, Node, PackageSpec, Version, VersionConstraint,
        copy_payload, digest, digest_reader, validate_digest, validate_name, validate_publisher,
        validate_summary,
    },
    systemd::{self, Action as SystemdAction},
};

/// Limit for a self-contained catalog archive: a root and all of its shards.
pub(crate) const CATALOG_LIMIT: u64 = 512 * 1024 * 1024;
pub(crate) const MANIFEST_LIMIT: u64 = 32 * 1024 * 1024;
const FILE_INDEX_OBJECTS: &str = "file-indexes";
const FILE_INDEX_LIMIT: u64 = 64 * 1024 * 1024;
const TMPFILES_LIMIT: u64 = 1024 * 1024;
const PUBLISH_POLICY_LIMIT: u64 = 1024 * 1024;
const PUBLISH_RELEASE_LIMIT: u64 = 1024 * 1024;
const PUBLISH_POLICY: &str = "publish-policy.kdl";

mod git_snapshots;
mod keys;
pub use keys::{KeySpecs, trust::TrustDraft};
use keys::{
    PublicKey, Signers,
    trust::{self, Freshness, TrustChain},
};
pub(crate) use keys::{Purpose, SigningKey};
pub(crate) mod profiles;
mod rebuild;
pub mod release;
mod shards;
mod source_archive;
pub(crate) use git_snapshots::{assign_git_version, lock_snapshot_series};
pub use rebuild::{export_rebuild_inputs, verify_rebuild};
pub(crate) use shards::{CATALOG_ROOT_LIMIT, SHARD_LIMIT, SHARD_OBJECTS};
use shards::{Shard, shard_of, shard_of_id};
pub use source_archive::{archive_build_sources, fetch_build_sources};

/// A publisher catalog: the signed root fields and the records of the shards
/// that were loaded. Catalogs read for clients are always complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Catalog {
    publisher: String,
    entries: BTreeMap<String, CatalogEntry>,
    file_index: bool,
    attestations: BTreeMap<String, String>,
    attestations_required: bool,
    toolchains_verified: bool,
    build_inputs: BTreeMap<String, rebuild::Digests>,
    source_archives_required: bool,
    withdrawals: BTreeMap<String, Withdrawal>,
    retired: BTreeMap<String, Retired>,
    pruned_through: u64,
    sequence: u64,
    profiles_required: bool,
    obsoletions: BTreeMap<String, Version>,
    renames: BTreeMap<String, String>,
    trust: TrustChain,
    /// Every key of every trust root version in `trust`, once verified.
    trust_keys: BTreeSet<PublicKey>,
    /// SHA-256 of the version 1 trust root payload, once verified.
    trust_genesis: String,
    release_profiles: BTreeMap<String, ReleaseProfile>,
    layout: Layout,
}

fn slot<'g>(
    grouped: &'g mut BTreeMap<String, Shard>,
    layout: &Layout,
    publisher: &str,
    prefix: String,
) -> Result<&'g mut Shard> {
    ensure!(
        layout.loaded.contains_key(&prefix) || !layout.committed.contains_key(&prefix),
        "catalog shard {prefix} was changed without being loaded"
    );
    Ok(grouped
        .entry(prefix)
        .or_insert_with_key(|prefix| Shard::new(publisher, prefix)))
}

/// Which signed shards back the in-memory records.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Layout {
    /// Shard digests committed by the root this catalog was read from.
    committed: BTreeMap<String, String>,
    /// Shards whose records were merged into the catalog.
    loaded: BTreeMap<String, Arc<Shard>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseProfile {
    pub architecture: String,
    pub facets: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variants: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub facet_overrides: BTreeSet<String>,
    pub companions: Vec<CatalogPin>,
    pub roots: BTreeMap<String, Version>,
    pub composition: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub elf_library_dirs: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub elf_symbol_versions: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub elf_require_declarations: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_percentage: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advisories: Vec<ReleaseAdvisory>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseAdvisory {
    pub id: String,
    pub package: String,
    pub publisher: String,
    pub affected: String,
    pub fixed: Version,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

impl ReleaseProfile {
    pub(crate) fn check_image<'a>(
        &self,
        name: &str,
        selection: crate::model::Selection<'_>,
        companions: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        ensure!(
            self.architecture == selection.arch
                && &self.facets == selection.facets
                && &self.variants == selection.variants,
            "image architecture, facets or variants differ from signed release profile {name}"
        );
        ensure!(
            self.companions
                .iter()
                .map(|pin| pin.publisher.as_str())
                .eq(companions),
            "image publisher order differs from signed release profile {name}"
        );
        Ok(())
    }

    fn validate_composition(&self, name: &str, catalog: &Catalog) -> Result<()> {
        ensure!(
            !self.composition.is_empty() && !self.roots.is_empty(),
            "signed profile {name} requires a recorded composition and roots"
        );
        if let Some(percentage) = self.rollout_percentage {
            ensure!(
                percentage <= 100,
                "signed profile {name} requires a rollout percentage from 0 to 100"
            );
        }
        for (package, manifest) in &self.composition {
            validate_name(package)?;
            validate_digest(manifest)?;
        }
        for (root, version) in &self.roots {
            validate_name(root)?;
            let id = format!("cps://{}/{root}@{version}", catalog.publisher);
            let entry = catalog.entries.get(&id).with_context(|| {
                format!("signed profile {name} references unavailable root {root}@{version}")
            })?;
            ensure!(
                entry.supports_architecture(&self.architecture),
                "signed profile {name} has an incompatible architecture for root {root}@{version}"
            );
            ensure!(
                self.composition.get(root).map(String::as_str) == Some(entry.manifest_digest()),
                "signed profile {name} has a different manifest for root {root}@{version}"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogPin {
    pub publisher: String,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_trust: Option<BuildTrust>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lineage: Option<CatalogLineage>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CatalogLineage {
    /// SHA-256 of the companion's version 1 trust root payload.
    trust_root_sha256: String,
    sequence: u64,
}

impl CatalogLineage {
    fn new(catalog: &Catalog) -> Self {
        Self {
            trust_root_sha256: catalog.trust_genesis.clone(),
            sequence: catalog.sequence,
        }
    }
}

impl CatalogPin {
    pub(crate) fn primary(publisher: String, sha256: String) -> Self {
        Self {
            publisher,
            sha256,
            build_trust: None,
            lineage: None,
        }
    }

    fn read(&self, repo: &Path, publisher: &str, key: &[u8]) -> Result<Catalog> {
        ensure!(
            self.publisher == publisher,
            "pinned publisher differs from {publisher}"
        );
        let catalog = read_catalog(repo, &self.sha256, publisher, key)?;
        self.check_lineage(&catalog)?;
        Ok(catalog)
    }

    fn check_lineage(&self, catalog: &Catalog) -> Result<()> {
        if let Some(lineage) = &self.lineage {
            ensure!(
                lineage == &CatalogLineage::new(catalog),
                "signed companion lineage differs from catalog {} for {}",
                self.sha256,
                self.publisher
            );
        }
        Ok(())
    }

    pub(crate) fn check_archive(&self, catalog: &Catalog, hash: &str) -> Result<()> {
        ensure!(
            self.publisher == catalog.publisher && self.sha256 == hash,
            "archived catalog for {} differs from the signed profile pin",
            self.publisher
        );
        self.check_lineage(catalog)
    }

    pub(crate) fn snapshot(&self, repo: &Path, publisher: &str, key: &[u8]) -> Result<Snapshot> {
        let catalog = self.read(repo, publisher, key)?;
        catalog_snapshot(repo, catalog, self.sha256.clone())
    }

    pub(crate) fn candidates(
        &self,
        repo: &Path,
        publisher: &str,
        key: &[u8],
    ) -> Result<CandidateSnapshot> {
        let catalog = self.read(repo, publisher, key)?;
        candidate_snapshot(repo, catalog, self.sha256.clone())
    }
}

impl Catalog {
    pub(crate) fn checkpoint(&self, catalog: String) -> CatalogCheckpoint {
        CatalogCheckpoint {
            catalog,
            sequence: self.sequence,
            trust: self.trust.clone(),
        }
    }

    fn empty(publisher: &str) -> Self {
        Self {
            publisher: publisher.to_owned(),
            entries: BTreeMap::new(),
            file_index: false,
            attestations: BTreeMap::new(),
            attestations_required: false,
            toolchains_verified: false,
            build_inputs: BTreeMap::new(),
            source_archives_required: false,
            withdrawals: BTreeMap::new(),
            retired: BTreeMap::new(),
            pruned_through: 0,
            sequence: 0,
            profiles_required: false,
            obsoletions: BTreeMap::new(),
            renames: BTreeMap::new(),
            trust: TrustChain::default(),
            trust_keys: BTreeSet::new(),
            trust_genesis: String::new(),
            release_profiles: BTreeMap::new(),
            layout: Layout::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn sequence(&self) -> u64 {
        self.sequence
    }

    /// True when the publisher has never published a package.
    fn is_unpublished(&self) -> bool {
        self.layout.committed.is_empty()
            && self.entries.is_empty()
            && self.withdrawals.is_empty()
            && self.retired.is_empty()
    }

    fn is_complete(&self) -> bool {
        self.layout
            .committed
            .keys()
            .all(|prefix| self.layout.loaded.contains_key(prefix))
    }

    /// The shard and file index objects of a complete catalog.
    fn metadata_objects(&self) -> Vec<(&'static str, String, u64)> {
        let mut objects = Vec::new();
        for (prefix, hash) in &self.layout.committed {
            objects.push((SHARD_OBJECTS, hash.clone(), SHARD_LIMIT));
            if let Some(index) = self
                .layout
                .loaded
                .get(prefix)
                .and_then(|shard| shard.file_index.as_ref())
            {
                objects.push((FILE_INDEX_OBJECTS, index.clone(), FILE_INDEX_LIMIT));
            }
        }
        objects
    }

    fn file_indexes(&self) -> Vec<String> {
        self.layout
            .loaded
            .values()
            .filter_map(|shard| shard.file_index.clone())
            .collect()
    }

    /// Write the shards whose records changed and return the root to sign.
    fn commit(&mut self, repo: &Path) -> Result<shards::Root> {
        let mut grouped = BTreeMap::<String, Shard>::new();
        let publisher = self.publisher.clone();
        for (id, entry) in &self.entries {
            slot(&mut grouped, &self.layout, &publisher, shard_of_id(id)?)?
                .entries
                .insert(id.clone(), entry.clone());
        }
        for (id, hash) in &self.attestations {
            slot(&mut grouped, &self.layout, &publisher, shard_of_id(id)?)?
                .attestations
                .insert(id.clone(), hash.clone());
        }
        for (id, inputs) in &self.build_inputs {
            slot(&mut grouped, &self.layout, &publisher, shard_of_id(id)?)?
                .build_inputs
                .insert(id.clone(), inputs.clone());
        }
        for (id, record) in &self.withdrawals {
            slot(&mut grouped, &self.layout, &publisher, shard_of_id(id)?)?
                .withdrawals
                .insert(id.clone(), record.clone());
        }
        for (id, record) in &self.retired {
            slot(&mut grouped, &self.layout, &publisher, shard_of_id(id)?)?
                .retired
                .insert(id.clone(), record.clone());
        }
        for (name, cutoff) in &self.obsoletions {
            slot(&mut grouped, &self.layout, &publisher, shard_of(name))?
                .obsoletions
                .insert(name.clone(), cutoff.clone());
        }
        let loaded = std::mem::take(&mut self.layout.loaded);
        for prefix in loaded.keys() {
            grouped
                .entry(prefix.clone())
                .or_insert_with(|| Shard::new(&publisher, prefix));
        }
        for (prefix, mut shard) in grouped {
            let previous = loaded.get(&prefix);
            if shard.is_empty() {
                self.layout.committed.remove(&prefix);
                continue;
            }
            if self.file_index {
                shard.file_index = index_shard(repo, previous.map(Arc::as_ref), &shard)?;
            }
            if let Some(previous) = previous.filter(|previous| ***previous == shard) {
                self.layout.loaded.insert(prefix, previous.clone());
                continue;
            }
            let bytes = shard.encode()?;
            let hash = store(repo, SHARD_OBJECTS, &bytes)?;
            let shard = Arc::new(shard);
            shards::remember(&hash, shard.clone());
            self.layout.committed.insert(prefix.clone(), hash);
            self.layout.loaded.insert(prefix, shard);
        }
        Ok(shards::Root {
            format: shards::ROOT_FORMAT,
            publisher: self.publisher.clone(),
            sequence: self.sequence,
            shards: self.layout.committed.clone(),
            file_index: self.file_index,
            attestations_required: self.attestations_required,
            toolchains_verified: self.toolchains_verified,
            source_archives_required: self.source_archives_required,
            profiles_required: self.profiles_required,
            pruned_through: self.pruned_through,
            renames: self.renames.clone(),
            trust: self.trust.clone(),
            release_profiles: self.release_profiles.clone(),
        })
    }

    /// Merge more of the committed shards into the catalog.
    fn load_shards(
        &mut self,
        prefixes: impl IntoIterator<Item = String>,
        mut read: impl FnMut(&str) -> Result<Arc<Shard>>,
    ) -> Result<()> {
        for prefix in prefixes {
            if self.layout.loaded.contains_key(&prefix) {
                continue;
            }
            let Some(hash) = self.layout.committed.get(&prefix).cloned() else {
                continue;
            };
            let shard = read(&hash)?;
            self.merge_shard(&prefix, &shard)
                .with_context(|| format!("checking catalog shard {hash}"))?;
            self.layout.loaded.insert(prefix, shard);
        }
        Ok(())
    }

    fn load_all(&mut self, repo: &Path) -> Result<()> {
        let prefixes: Vec<_> = self.layout.committed.keys().cloned().collect();
        self.load_shards(prefixes, |hash| shards::read_shard(repo, hash))?;
        self.validate_complete()
    }

    /// Load the shards that hold these package names and every rename record.
    fn load_names<'a>(
        &mut self,
        repo: &Path,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        let mut prefixes: BTreeSet<_> = names.into_iter().map(shard_of).collect();
        for (name, to) in &self.renames {
            prefixes.insert(shard_of(name));
            prefixes.insert(shard_of(to));
        }
        self.load_shards(prefixes, |hash| shards::read_shard(repo, hash))
    }

    /// Check one shard against its root and add its records.
    fn merge_shard(&mut self, prefix: &str, shard: &Shard) -> Result<()> {
        ensure!(
            shard.publisher == self.publisher && shard.prefix == prefix,
            "catalog shard belongs to another publisher or prefix"
        );
        let publisher = self.publisher.as_str();
        for (id, entry) in &shard.entries {
            let spec = PackageSpec::parse(id)?;
            let version = spec
                .version
                .with_context(|| format!("catalog entry {id} has no version"))?;
            ensure!(
                shard
                    .obsoletions
                    .get(spec.name)
                    .is_none_or(|cutoff| version.semantic() > cutoff.semantic()),
                "catalog advertises obsolete package {id}"
            );
            ensure!(
                !self.attestations_required || shard.attestations.contains_key(id),
                "protected catalog contains a package without build evidence"
            );
            entry.validate()?;
        }
        for (name, cutoff) in &shard.obsoletions {
            let id = format!("cps://{publisher}/{name}@{cutoff}");
            ensure!(
                !shard.withdrawals.contains_key(&id) && !shard.retired.contains_key(&id),
                "obsoletion for {name} reuses a published version"
            );
        }
        for (id, digest) in &shard.attestations {
            validate_digest(digest)?;
            ensure!(
                shard.entries.contains_key(id),
                "attestation for absent package {id}"
            );
        }
        for (id, withdrawal) in &shard.withdrawals {
            ensure!(
                !shard.entries.contains_key(id) && withdrawal.sequence <= self.sequence,
                "invalid withdrawal for {id}"
            );
            withdrawal.manifest.validate()?;
            ensure!(
                !self.toolchains_verified || withdrawal.attestation.is_some(),
                "toolchain-verified catalog withdrew a package without build evidence"
            );
            if let Some(digest) = &withdrawal.attestation {
                validate_digest(digest)?;
            }
        }
        for (id, retired) in &shard.retired {
            ensure!(
                retired.withdrawn <= self.pruned_through,
                "retired package {id} was withdrawn after the pruning cutoff"
            );
        }
        for (id, inputs) in &shard.build_inputs {
            let attested = shard.attestations.contains_key(id)
                || shard
                    .withdrawals
                    .get(id)
                    .is_some_and(|withdrawal| withdrawal.attestation.is_some());
            ensure!(
                self.attestations_required && attested,
                "rebuild inputs require an admitted build attestation for {id}"
            );
            for (hash, _) in inputs.objects() {
                validate_digest(hash).context("invalid rebuild input digest")?;
            }
        }
        if self.source_archives_required {
            for id in shard.entries.keys().chain(shard.withdrawals.keys()) {
                ensure!(
                    shard
                        .build_inputs
                        .get(id)
                        .is_some_and(|inputs| inputs.sources_archived),
                    "protected catalog requires a source archive for {id}"
                );
            }
        }
        fn extend<T: Clone>(map: &mut BTreeMap<String, T>, records: &BTreeMap<String, T>) {
            map.extend(
                records
                    .iter()
                    .map(|(id, record)| (id.clone(), record.clone())),
            );
        }
        extend(&mut self.entries, &shard.entries);
        extend(&mut self.attestations, &shard.attestations);
        extend(&mut self.build_inputs, &shard.build_inputs);
        extend(&mut self.withdrawals, &shard.withdrawals);
        extend(&mut self.retired, &shard.retired);
        extend(&mut self.obsoletions, &shard.obsoletions);
        Ok(())
    }

    /// Checks that span shards, run once every shard is loaded.
    fn validate_complete(&self) -> Result<()> {
        ensure!(self.is_complete(), "catalog shards are incomplete");
        // The packages of one build share its attestation. Each package's
        // verification checks that the statement names it and its exact manifest.
        self.rename_targets()?;
        for (name, profile) in &self.release_profiles {
            profile.validate_composition(name, self)?;
        }
        Ok(())
    }

    fn package_record(&self, id: &str) -> Option<(&CatalogEntry, Option<&str>)> {
        if let Some(entry) = self.entries.get(id) {
            Some((entry, self.attestations.get(id).map(String::as_str)))
        } else {
            let withdrawn = self.withdrawals.get(id)?;
            Some((&withdrawn.manifest, withdrawn.attestation.as_deref()))
        }
    }

    pub(crate) fn is_withdrawn(&self, id: &str) -> bool {
        self.withdrawals.contains_key(id) || self.retired.contains_key(id)
    }

    fn check_source_archives(&self) -> Result<()> {
        if self.source_archives_required {
            ensure!(
                self.attestations_required,
                "source archive protection requires signed build evidence"
            );
            for id in self.entries.keys().chain(self.withdrawals.keys()) {
                ensure!(
                    self.build_inputs
                        .get(id)
                        .is_some_and(|inputs| inputs.sources_archived),
                    "protected catalog requires a source archive for {id}"
                );
            }
        }
        Ok(())
    }

    pub(crate) fn is_active(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    pub(crate) fn check_installed(&self, package: &Published) -> Result<Option<&str>> {
        let id = package.manifest.id();
        if let Some(retired) = self.retired.get(&id) {
            // Pruned history keeps the manifest binding, not the build evidence.
            ensure!(
                retired.manifest == package.sha256,
                "installed manifest digest differs from the signed catalog"
            );
            return Ok(None);
        }
        let (entry, attestation) = self
            .package_record(&id)
            .context("package is absent from the signed catalog")?;
        ensure!(
            entry.manifest_digest() == package.sha256,
            "installed manifest digest differs from the signed catalog"
        );
        entry.check_manifest(&package.manifest)?;
        Ok(attestation)
    }

    /// The newest trust root version the catalog is signed under.
    pub(crate) fn latest_trust_root(&self) -> Result<Vec<u8>> {
        self.trust
            .versions()
            .last()
            .context("catalog carries no trust root")?
            .encode()
    }

    /// Every root and catalog key in the catalog's verified trust history.
    pub(crate) fn publisher_keys(&self) -> Vec<PublicKey> {
        self.trust_keys.iter().copied().collect()
    }

    fn take_build_evidence(&mut self, hash: &str) -> Result<BuildEvidence> {
        let publisher_keys = self.publisher_keys();
        let mut attestations = std::mem::take(&mut self.attestations);
        attestations.extend(
            self.withdrawals
                .iter()
                .filter_map(|(id, withdrawn)| Some((id.clone(), withdrawn.attestation.clone()?))),
        );
        Ok(BuildEvidence {
            attestations,
            publisher_keys,
            history: CatalogCheckpoint {
                catalog: hash.to_owned(),
                sequence: self.sequence,
                trust: std::mem::take(&mut self.trust),
            },
        })
    }

    fn withdraw(&mut self, id: &str) -> Result<bool> {
        if self.is_withdrawn(id) {
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
                sequence: self.next_sequence()?.get(),
            },
        );
        Ok(true)
    }

    /// Replace withdrawal records from catalogs up to `through` with compact
    /// tombstones. Records that a signed profile composition names are kept.
    fn prune_withdrawals(&mut self, through: u64) -> Result<usize> {
        ensure!(
            through >= self.pruned_through,
            "pruning cutoff must not move back from sequence {}",
            self.pruned_through
        );
        ensure!(
            through <= self.sequence,
            "pruning cutoff is beyond the current catalog sequence {}",
            self.sequence
        );
        let referenced: BTreeSet<_> = self
            .release_profiles
            .values()
            .flat_map(|profile| profile.composition.values())
            .collect();
        let pruned: Vec<_> = self
            .withdrawals
            .iter()
            .filter(|(_, record)| {
                record.sequence <= through && !referenced.contains(&record.manifest.0.manifest)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &pruned {
            let record = self.withdrawals.remove(id).expect("selected withdrawal");
            self.build_inputs.remove(id);
            self.retired.insert(
                id.clone(),
                Retired {
                    manifest: record.manifest.manifest_digest().to_owned(),
                    withdrawn: record.sequence,
                },
            );
        }
        self.pruned_through = through;
        Ok(pruned.len())
    }

    fn obsolete(&mut self, id: &str) -> Result<bool> {
        let spec = PackageSpec::parse(id)?;
        ensure!(
            spec.publisher == Some(&self.publisher),
            "obsoletion belongs to a different publisher"
        );
        ensure!(
            !self.renames.contains_key(spec.name),
            "cannot change the signed rename cutoff for {}",
            spec.name
        );
        let cutoff = spec.version.context("obsoletion requires a version")?;
        if let Some(previous) = self.obsoletions.get(spec.name) {
            ensure!(
                cutoff.semantic() >= previous.semantic(),
                "obsoletion must advance the cutoff for {}",
                spec.name
            );
            if cutoff.semantic() == previous.semantic() {
                return Ok(false);
            }
        }
        let prefix = format!("cps://{}/{}@", self.publisher, spec.name);
        let active = self
            .entries
            .range(prefix.clone()..)
            .map(|(id, _)| id)
            .take_while(|id| id.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>();
        let retired = self
            .withdrawals
            .range(prefix.clone()..)
            .map(|(id, _)| id)
            .take_while(|id| id.starts_with(&prefix))
            .chain(
                self.retired
                    .range(prefix.clone()..)
                    .map(|(id, _)| id)
                    .take_while(|id| id.starts_with(&prefix)),
            );
        ensure!(
            !active.is_empty() || retired.clone().next().is_some(),
            "no published versions to obsolete for {}",
            spec.name
        );
        for previous in active.iter().chain(retired) {
            let version = PackageSpec::parse(previous)?
                .version
                .context("published package has no version")?;
            ensure!(
                cutoff.semantic() > version.semantic(),
                "obsoletion {id} must be newer than {previous}"
            );
        }
        for previous in active {
            self.withdraw(&previous)?;
        }
        self.obsoletions.insert(spec.name.to_owned(), cutoff);
        Ok(true)
    }

    fn rename(&mut self, id: &str, to: String) -> Result<bool> {
        let spec = PackageSpec::parse(id)?;
        if let Some(existing) = self.renames.get(spec.name) {
            ensure!(
                existing == &to,
                "cannot change the signed rename for {}",
                spec.name
            );
            ensure!(
                self.obsoletions.get(spec.name) == spec.version.as_ref(),
                "cannot change the signed rename cutoff for {}",
                spec.name
            );
            return Ok(false);
        }
        self.obsolete(id)?;
        self.renames.insert(spec.name.to_owned(), to);
        Ok(true)
    }

    pub(crate) fn rename_targets(&self) -> Result<BTreeMap<String, String>> {
        if self.renames.is_empty() {
            return Ok(BTreeMap::new());
        }
        let active = self
            .entries
            .keys()
            .map(|id| Ok(PackageSpec::parse(id)?.name))
            .collect::<Result<BTreeSet<_>>>()?;
        let mut published = active.clone();
        for id in self.withdrawals.keys().chain(self.retired.keys()) {
            published.insert(PackageSpec::parse(id)?.name);
        }
        for (name, to) in &self.renames {
            validate_name(name)?;
            validate_name(to)?;
            let cutoff = self
                .obsoletions
                .get(name)
                .with_context(|| format!("rename of {name} has no obsoletion cutoff"))?;
            ensure!(
                !active.contains(name.as_str()),
                "renamed package {name} is still published"
            );
            let prefix = format!("cps://{}/{name}@", self.publisher);
            let mut withdrawn = self
                .withdrawals
                .range(prefix.clone()..)
                .map(|(id, _)| id)
                .take_while(|id| id.starts_with(&prefix))
                .chain(
                    self.retired
                        .range(prefix.clone()..)
                        .map(|(id, _)| id)
                        .take_while(|id| id.starts_with(&prefix)),
                )
                .peekable();
            ensure!(
                withdrawn.peek().is_some(),
                "no published versions to rename for {name}"
            );
            for id in withdrawn {
                ensure!(
                    PackageSpec::parse(id)?
                        .version
                        .is_some_and(|version| version.semantic() < cutoff.semantic()),
                    "rename cutoff for {name} must be newer than {id}"
                );
            }
        }
        let mut resolved = BTreeMap::<&str, &str>::new();
        for name in self.renames.keys() {
            let mut chain = BTreeSet::new();
            let mut target = name.as_str();
            while let Some(to) = self.renames.get(target) {
                if let Some(&terminal) = resolved.get(target) {
                    target = terminal;
                    break;
                }
                ensure!(
                    chain.insert(target),
                    "package rename cycle involving {target}"
                );
                target = to;
            }
            ensure!(
                published.contains(target),
                "rename of {name} has no published successor {target}"
            );
            resolved.extend(chain.into_iter().map(|name| (name, target)));
        }
        Ok(resolved
            .into_iter()
            .map(|(name, to)| (name.to_owned(), to.to_owned()))
            .collect())
    }

    pub(crate) fn check_successor(&self, next: &Self, _previous_hash: &str) -> Result<()> {
        check_catalog_sequence(self.sequence, next.sequence)?;
        self.trust.check_successor(&next.trust)?;
        ensure!(
            !self.file_index || next.file_index,
            "release drops the publisher's file index"
        );
        ensure!(
            next.pruned_through >= self.pruned_through,
            "release moves the withdrawal pruning cutoff back"
        );
        ensure!(
            !self.attestations_required || next.attestations_required,
            "release disables the publisher's build evidence requirement"
        );
        ensure!(
            !self.toolchains_verified || next.toolchains_verified,
            "release disables the publisher's toolchain verification requirement"
        );
        ensure!(
            !self.source_archives_required || next.source_archives_required,
            "release disables the publisher's source archive requirement"
        );
        ensure!(
            !next.toolchains_verified || self.toolchains_verified || self.is_unpublished(),
            "release claims toolchain verification after packages were published"
        );
        ensure!(
            !self.profiles_required || next.profiles_required,
            "release disables the publisher's composition profile requirement"
        );
        ensure!(
            self.release_profiles.is_empty() || !next.release_profiles.is_empty(),
            "release drops signed composition profiles"
        );
        self.check_profile_guards(&next.release_profiles)?;
        // A withdrawal may only become a tombstone through the signed pruning
        // cutoff, and a tombstone keeps the manifest it replaced.
        let pruned = |id: &str, manifest: &str, withdrawn: std::ops::RangeInclusive<u64>| {
            next.retired.get(id).is_some_and(|retired| {
                retired.manifest == manifest
                    && withdrawn.contains(&retired.withdrawn)
                    && retired.withdrawn <= next.pruned_through
            })
        };
        for (id, record) in &self.withdrawals {
            ensure!(
                next.withdrawals.get(id) == Some(record)
                    || pruned(
                        id,
                        record.manifest.manifest_digest(),
                        record.sequence..=record.sequence
                    ),
                "release drops or changes the withdrawal for {id}"
            );
        }
        for (id, retired) in &self.retired {
            ensure!(
                next.retired.get(id) == Some(retired),
                "release drops or changes the retired record for {id}"
            );
        }
        for (name, cutoff) in &self.obsoletions {
            ensure!(
                next.obsoletions
                    .get(name)
                    .is_some_and(|next| next.semantic() >= cutoff.semantic()),
                "release drops or lowers the obsoletion for {name}"
            );
        }
        for (name, to) in &self.renames {
            ensure!(
                next.renames.get(name) == Some(to)
                    && next.obsoletions.get(name) == self.obsoletions.get(name),
                "release drops or changes the signed rename for {name}"
            );
        }
        for (id, manifest) in &self.entries {
            if pruned(
                id,
                manifest.manifest_digest(),
                self.sequence.saturating_add(1)..=u64::MAX,
            ) {
                continue;
            }
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
        for (id, inputs) in &self.build_inputs {
            ensure!(
                next.retired.contains_key(id)
                    || next
                        .build_inputs
                        .get(id)
                        .is_some_and(|next| inputs.preserved_by(next)),
                "release drops or changes the rebuild inputs for {id}"
            );
        }
        if !self.build_inputs.is_empty() {
            for id in next.entries.keys().chain(next.withdrawals.keys()) {
                ensure!(
                    self.package_record(id).is_some() || next.build_inputs.contains_key(id),
                    "release omits rebuild inputs for new package {id}"
                );
            }
        }
        Ok(())
    }

    fn check_profile_guards(&self, profiles: &BTreeMap<String, ReleaseProfile>) -> Result<()> {
        for (name, profile) in &self.release_profiles {
            let next = profiles.get(name);
            if !profile.elf_library_dirs.is_empty() {
                let next = next
                    .filter(|next| !next.elf_library_dirs.is_empty())
                    .with_context(|| {
                        format!("release drops ELF checking for signed profile {name}")
                    })?;
                ensure!(
                    !profile.elf_symbol_versions || next.elf_symbol_versions,
                    "release drops ELF symbol version checking for signed profile {name}"
                );
                ensure!(
                    !profile.elf_require_declarations || next.elf_require_declarations,
                    "release drops ELF dependency declaration checking for signed profile {name}"
                );
            }
            if !profile.roots.is_empty() {
                let next = next
                    .filter(|next| !next.roots.is_empty())
                    .with_context(|| {
                        format!("release drops the composition roots for signed profile {name}")
                    })?;
                ensure!(
                    profile.architecture == next.architecture
                        && profile.facets == next.facets
                        && profile.variants == next.variants
                        && profile
                            .companions
                            .iter()
                            .map(|pin| &pin.publisher)
                            .eq(next.companions.iter().map(|pin| &pin.publisher)),
                    "release changes image configuration for signed profile {name}"
                );
            }
            ensure!(
                profile.composition.is_empty()
                    || next.is_some_and(|next| !next.composition.is_empty()),
                "release drops the checked composition for signed profile {name}"
            );
            for pin in &profile.companions {
                let candidate = next.and_then(|next| {
                    next.companions
                        .iter()
                        .find(|next| next.publisher == pin.publisher)
                });
                if pin.build_trust.is_some() {
                    ensure!(
                        candidate.is_some_and(|next| next.build_trust.is_some()),
                        "release drops companion build trust for {} in signed profile {name}",
                        pin.publisher
                    );
                }
                if let Some(previous) = &pin.lineage {
                    let (candidate, lineage) = candidate
                        .and_then(|pin| pin.lineage.as_ref().map(|lineage| (pin, lineage)))
                        .with_context(|| {
                            format!(
                                "release drops companion lineage for {} in signed profile {name}",
                                pin.publisher
                            )
                        })?;
                    ensure!(
                        previous.trust_root_sha256 == lineage.trust_root_sha256,
                        "release changes companion trust anchor for {} in signed profile {name}",
                        pin.publisher
                    );
                    ensure!(
                        if candidate.sha256 == pin.sha256 {
                            lineage.sequence == previous.sequence
                        } else {
                            lineage.sequence > previous.sequence
                        },
                        "companion catalog rollback or fork for {} in signed profile {name}",
                        pin.publisher
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_profile_build_trust(&self) -> Result<()> {
        for pin in self
            .release_profiles
            .values()
            .flat_map(|profile| &profile.companions)
        {
            if let Some(trust) = &pin.build_trust {
                trust.validate()?;
                for key in &self.trust_keys {
                    trust.check_publisher_key(key)?;
                }
            }
        }
        Ok(())
    }

    fn next_sequence(&self) -> Result<NonZeroU64> {
        self.sequence
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .context("publisher catalog sequence exhausted")
    }
}

/// A signed catalog record for one package version. Besides the manifest
/// digest it carries what the resolver reads, so planning needs no manifests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CatalogEntry(Arc<EntryData>);

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct EntryData {
    manifest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    architecture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    depends: Vec<Dependency>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    provides: BTreeMap<String, Option<Version>>,
}

impl Serialize for CatalogEntry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CatalogEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        EntryData::deserialize(deserializer).map(|data| Self(Arc::new(data)))
    }
}

impl CatalogEntry {
    fn new(sha256: String, manifest: &Manifest) -> Self {
        Self(Arc::new(EntryData {
            manifest: sha256,
            architecture: manifest.architecture.clone(),
            summary: manifest.summary.clone(),
            depends: manifest.dependency.clone(),
            provides: manifest.provides.clone(),
        }))
    }

    pub(crate) fn manifest_digest(&self) -> &str {
        &self.0.manifest
    }

    fn architecture(&self) -> Option<&str> {
        self.0.architecture.as_deref()
    }

    pub(crate) fn summary(&self) -> Option<&str> {
        self.0.summary.as_deref()
    }

    pub(crate) fn dependencies(&self) -> &[Dependency] {
        &self.0.depends
    }

    pub(crate) fn provides(&self) -> &BTreeMap<String, Option<Version>> {
        &self.0.provides
    }

    pub(crate) fn supports_architecture(&self, architecture: &str) -> bool {
        self.architecture()
            .is_none_or(|target| target == architecture)
    }

    fn validate(&self) -> Result<()> {
        validate_digest(self.manifest_digest())?;
        if let Some(architecture) = self.architecture() {
            validate_publisher(architecture).context("invalid catalog package architecture")?;
        }
        if let Some(summary) = self.summary() {
            validate_summary(summary)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn digest_only(sha256: String) -> Self {
        Self(Arc::new(EntryData {
            manifest: sha256,
            architecture: None,
            summary: None,
            depends: Vec::new(),
            provides: BTreeMap::new(),
        }))
    }

    /// Bind the signed summary to the manifest it describes.
    fn check_manifest(&self, manifest: &Manifest) -> Result<()> {
        ensure!(
            self.architecture() == manifest.architecture.as_deref(),
            "catalog architecture differs from the manifest for {}",
            manifest.id()
        );
        ensure!(
            self.summary() == manifest.summary.as_deref(),
            "catalog summary differs from the manifest for {}",
            manifest.id()
        );
        ensure!(
            self.0.depends == manifest.dependency && self.0.provides == manifest.provides,
            "catalog dependency summary differs from the manifest for {}",
            manifest.id()
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Withdrawal {
    manifest: CatalogEntry,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attestation: Option<String>,
    /// The catalog sequence that first withdrew the package.
    sequence: u64,
}

/// What remains of a withdrawal after a signed pruning: the manifest binding,
/// so the ID cannot be reused and installed copies still verify.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Retired {
    manifest: String,
    withdrawn: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Published {
    pub sha256: String,
    pub manifest: Manifest,
    document: String,
}

impl Published {
    pub fn verify(&self) -> Result<()> {
        self.manifest.validate(true)?;
        ensure!(
            parse_manifest(&self.document)? == self.manifest,
            "stored manifest does not match its KDL document"
        );
        ensure!(
            digest(self.document.as_bytes()) == self.sha256,
            "installed manifest digest mismatch for {}",
            self.manifest.id()
        );
        Ok(())
    }
}

fn load_manifest(repo: &Path, sha256: String) -> Result<Published> {
    let bytes = read_object(repo, "manifests", &sha256, MANIFEST_LIMIT)?;
    let document = String::from_utf8(bytes)?;
    let manifest = parse_manifest(&document)?;
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
    pub(crate) builds: BuildEvidence,
    build_inputs: BTreeMap<String, rebuild::Digests>,
    /// Shards and file indexes: (object kind, digest, size limit).
    metadata: Vec<(&'static str, String, u64)>,
    pub(crate) obsoletions: BTreeMap<String, Version>,
    pub(crate) rename_targets: BTreeMap<String, String>,
}

pub(crate) struct BuildEvidence {
    attestations: BTreeMap<String, String>,
    publisher_keys: Vec<[u8; 32]>,
    pub(crate) history: CatalogCheckpoint,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogCheckpoint {
    pub(crate) catalog: String,
    sequence: u64,
    /// The trust root history of the catalog: the highest verified version.
    trust: TrustChain,
}

impl CatalogCheckpoint {
    pub(crate) fn read_archive(
        &self,
        bytes: &[u8],
        publisher: &str,
        key: &[u8],
    ) -> Result<Catalog> {
        let catalog = decode_archived_catalog(bytes, &self.catalog, publisher, key)?;
        ensure!(
            self.sequence == catalog.sequence && self.trust == catalog.trust,
            "catalog checkpoint metadata mismatch"
        );
        Ok(catalog)
    }

    pub(crate) fn check(&self, next: &Self) -> Result<()> {
        if self.catalog == next.catalog {
            ensure!(self == next, "catalog checkpoint metadata mismatch");
            return Ok(());
        }
        check_catalog_sequence(self.sequence, next.sequence)?;
        self.trust.check_successor(&next.trust)
    }
}

fn check_catalog_sequence(previous: u64, next: u64) -> Result<()> {
    ensure!(
        next > previous,
        "publisher catalog rollback or fork after sequence {previous}"
    );
    Ok(())
}

/// Local record of an authenticated catalog's admission of a manifest.
/// The envelope and publisher trust root history are independently verifiable.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuildReceipt {
    pub publisher: String,
    pub manifest: String,
    attestation: String,
    history: CatalogCheckpoint,
}

impl BuildReceipt {
    fn new(package: &Published, attestation: &str, history: &CatalogCheckpoint) -> Self {
        Self {
            publisher: package.manifest.publisher.clone(),
            manifest: package.sha256.clone(),
            attestation: attestation.to_owned(),
            history: history.clone(),
        }
    }

    pub(crate) fn read_attestation(&self, repo: &Path) -> Result<Vec<u8>> {
        read_object(repo, "attestations", &self.attestation, ATTESTATION_LIMIT)
    }

    pub(crate) fn catalog(&self) -> &str {
        &self.history.catalog
    }

    pub(crate) fn check_attestation(&self, expected: &str) -> Result<()> {
        ensure!(
            self.attestation == expected,
            "archived build attestation differs from the signed catalog"
        );
        Ok(())
    }

    pub(crate) fn verify(
        &self,
        bytes: &[u8],
        package: &Published,
        anchor: &[u8],
        trust: &BuildTrust,
    ) -> Result<()> {
        validate_digest(&self.history.catalog)?;
        ensure!(
            self.manifest == package.sha256 && self.publisher == package.manifest.publisher,
            "archived build receipt belongs to a different package"
        );
        ensure!(
            digest(bytes) == self.attestation,
            "archived build attestation digest mismatch"
        );
        let history = trust::verify(
            &self.publisher,
            &self.history.trust,
            anchor,
            Freshness::Pinned,
        )?;
        for key in history.keys() {
            trust.check_publisher_key(&key)?;
        }
        package.verify()?;
        verify_package_attestation(bytes, trust, package)
    }
}

impl BuildEvidence {
    pub(crate) fn check_receipt(&self, package: &Published, receipt: &BuildReceipt) -> Result<()> {
        let id = package.manifest.id();
        let expected = self
            .attestations
            .get(&id)
            .with_context(|| format!("no build attestation for {id}"))?;
        receipt.check_attestation(expected)
    }

    pub(crate) fn verify_policies(
        &self,
        repo: &Path,
        package: &Published,
        signed: Option<&BuildTrust>,
        local: Option<&BuildTrust>,
    ) -> Result<Option<BuildReceipt>> {
        let local = local.filter(|trust| Some(*trust) != signed);
        if signed.is_none() && local.is_none() {
            return Ok(None);
        }
        self.verify(repo, signed.into_iter().chain(local), package)
            .map(Some)
    }

    pub(crate) fn check_keys(&self, trust: &BuildTrust) -> Result<()> {
        for key in &self.publisher_keys {
            trust.check_publisher_key(key)?;
        }
        Ok(())
    }

    pub(crate) fn verify<'a>(
        &self,
        repo: &Path,
        policies: impl IntoIterator<Item = &'a BuildTrust>,
        package: &Published,
    ) -> Result<BuildReceipt> {
        let mut policies = policies.into_iter().peekable();
        ensure!(policies.peek().is_some(), "missing builder policy");
        let id = package.manifest.id();
        let hash = self
            .attestations
            .get(&id)
            .with_context(|| format!("no build attestation for {id}"))?;
        let bytes = read_object(repo, "attestations", hash, ATTESTATION_LIMIT)
            .with_context(|| format!("reading build attestation for {id}"))?;
        for trust in policies {
            self.check_keys(trust)?;
            verify_package_attestation(&bytes, trust, package)?;
        }
        Ok(BuildReceipt::new(package, hash, &self.history))
    }
}

fn verify_repository_attestation(
    repo: &Path,
    trust: &BuildTrust,
    package: &Published,
    hash: &str,
) -> Result<()> {
    let id = package.manifest.id();
    let bytes = read_object(repo, "attestations", hash, ATTESTATION_LIMIT)
        .with_context(|| format!("reading build attestation for {id}"))?;
    verify_package_attestation(&bytes, trust, package)
}

fn verify_package_attestation(bytes: &[u8], trust: &BuildTrust, package: &Published) -> Result<()> {
    trust
        .verify(
            bytes,
            &package.manifest,
            FileDigest {
                sha256: package.sha256.clone(),
                size: package.document.len() as u64,
            },
        )
        .with_context(|| format!("verifying build evidence for {}", package.manifest.id()))
}

#[derive(Default)]
pub struct BuildAudit {
    pub verified: usize,
    pub failures: Vec<String>,
    pub(crate) receipts: Vec<BuildReceipt>,
}

pub(crate) fn audit_installed_builds<'a>(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    pin: Option<&CatalogPin>,
    check: impl FnOnce(&CatalogCheckpoint) -> Result<()>,
    trust: &BuildTrust,
    installed: impl IntoIterator<Item = &'a Published>,
) -> Result<BuildAudit> {
    let (catalog, hash) = read_selected_catalog(repo, publisher, key, pin)?;
    for key in catalog.publisher_keys() {
        trust.check_publisher_key(&key)?;
    }
    let mut audit = BuildAudit::default();
    let history = catalog.checkpoint(hash);
    check(&history)?;
    for package in installed {
        let id = package.manifest.id();
        let installed_record = || {
            package.verify()?;
            catalog
                .check_installed(package)?
                .with_context(|| format!("no build attestation for {id}"))
        };
        let result = installed_record()
            .with_context(|| format!("checking installed manifest for {id}"))
            .and_then(|attestation| {
                verify_repository_attestation(repo, trust, package, attestation)?;
                Ok(BuildReceipt::new(package, attestation, &history))
            });
        match result {
            Ok(receipt) => {
                audit.verified += 1;
                audit.receipts.push(receipt);
            }
            Err(error) => audit.failures.push(format!("{error:#}")),
        }
    }
    Ok(audit)
}

const PUBLIC_KEY_DER_LEN: u64 = 44;

fn read_key_der(path: &Path) -> Result<Vec<u8>> {
    (|| read_limited(File::open(path)?, PUBLIC_KEY_DER_LEN))()
        .with_context(|| format!("reading trust key {}", path.display()))
}

/// Read a raw Ed25519 public key from a DER file, such as a builder key.
pub fn read_key(path: &Path) -> Result<Vec<u8>> {
    parse_public_key(&read_key_der(path)?).map(<[u8]>::to_vec)
}

/// Read a publisher's pinned trust root file. Its signatures are checked
/// against its own root keys and its canonical bytes are returned.
pub fn read_trust_root(path: &Path) -> Result<Vec<u8>> {
    trust::read_pinned(path)
}

/// Every key a pinned trust root names, for checks that builder keys differ.
pub(crate) fn trust_root_keys(pinned: &[u8]) -> Result<Vec<PublicKey>> {
    Ok(trust::pinned_keys(pinned)?.into_iter().collect())
}

fn parse_public_key(bytes: &[u8]) -> Result<&[u8]> {
    // SubjectPublicKeyInfo for Ed25519, as emitted by openssl pkey -pubout -outform DER.
    const PREFIX: &[u8] = b"\x30\x2a\x30\x05\x06\x03\x2b\x65\x70\x03\x21\x00";
    let key = bytes
        .strip_prefix(PREFIX)
        .context("expected an Ed25519 DER public key")?;
    ensure!(key.len() == 32, "invalid Ed25519 public key length");
    Ok(key)
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

/// Read a catalog the client already pins, or a historical one. `key` is
/// the client's pinned trust root.
pub(crate) fn read_catalog(
    repo: &Path,
    hash: &str,
    publisher: &str,
    key: &[u8],
) -> Result<Catalog> {
    read_catalog_with_freshness(repo, hash, publisher, key, Freshness::Pinned)
}

/// Read the repository's current catalog, whose trust root must be unexpired.
pub(crate) fn read_current_catalog(
    repo: &Path,
    publisher: &str,
    key: &[u8],
) -> Result<(Catalog, String)> {
    let hash = read_current(repo)?;
    let catalog = read_catalog_with_freshness(repo, &hash, publisher, key, Freshness::Current)?;
    Ok((catalog, hash))
}

/// Read a catalog a mirror advertises as current, whose trust root must be
/// unexpired. `key` is the client's pinned trust root.
pub(crate) fn read_fresh_catalog(
    repo: &Path,
    hash: &str,
    publisher: &str,
    key: &[u8],
) -> Result<Catalog> {
    read_catalog_with_freshness(repo, hash, publisher, key, Freshness::Current)
}

/// The trust root version the repository's current catalog is signed under,
/// verified against the client's pinned trust root without loading shards.
/// It may have expired.
pub(crate) fn current_trust_root(
    repo: &Path,
    publisher: &str,
    key: &[u8],
) -> Result<trust::TrustRoot> {
    let hash = read_current(repo)?;
    let catalog = read_root(repo, &hash, publisher, key, Freshness::Pinned)?;
    Ok(catalog.trust.verify(publisher)?.current().clone())
}

fn read_catalog_with_freshness(
    repo: &Path,
    hash: &str,
    publisher: &str,
    key: &[u8],
    freshness: Freshness,
) -> Result<Catalog> {
    let mut catalog = read_root(repo, hash, publisher, key, freshness)?;
    catalog.load_all(repo)?;
    Ok(catalog)
}

/// Read only the signed root of a catalog; no shard records are loaded.
fn read_root(
    repo: &Path,
    hash: &str,
    publisher: &str,
    key: &[u8],
    freshness: Freshness,
) -> Result<Catalog> {
    let bytes = read_object(repo, "catalogs", hash, CATALOG_ROOT_LIMIT)?;
    decode_root(&bytes, hash, publisher, key, freshness)
}

fn root_shards(repo: &Path, hash: &str) -> Result<BTreeMap<String, String>> {
    let bytes = read_object(repo, "catalogs", hash, CATALOG_ROOT_LIMIT)?;
    Ok(shards::Root::decode(&shards::open(&bytes)?.payload)?.shards)
}

/// The metadata objects a stored catalog root commits to, besides itself:
/// its shards and, when enabled, their file indexes.
pub(crate) fn catalog_objects(repo: &Path, hash: &str) -> Result<Vec<(&'static str, String, u64)>> {
    let mut objects = Vec::new();
    for shard in root_shards(repo, hash)?.into_values() {
        if let Some(index) = shards::read_shard(repo, &shard)?.file_index.clone() {
            objects.push((FILE_INDEX_OBJECTS, index, FILE_INDEX_LIMIT));
        }
        objects.push((SHARD_OBJECTS, shard, SHARD_LIMIT));
    }
    Ok(objects)
}

/// The shard digests a stored catalog root commits to.
pub(crate) fn catalog_shards(repo: &Path, hash: &str) -> Result<Vec<String>> {
    Ok(root_shards(repo, hash)?.into_values().collect())
}

/// A self-contained archive of a catalog: its root and every shard.
pub(crate) fn catalog_bytes(repo: &Path, hash: &str) -> Result<Vec<u8>> {
    let root = read_object(repo, "catalogs", hash, CATALOG_ROOT_LIMIT)?;
    let shards = shards::Root::decode(&shards::open(&root)?.payload)?
        .shards
        .into_values()
        .map(|shard| read_object(repo, SHARD_OBJECTS, &shard, SHARD_LIMIT))
        .collect::<Result<Vec<_>>>()?;
    let archive = shards::pack_archive(&root, shards.iter().map(Vec::as_slice))?;
    ensure!(
        archive.len() as u64 <= CATALOG_LIMIT,
        "catalog archive exceeds {CATALOG_LIMIT} bytes"
    );
    Ok(archive)
}

/// The catalog identity of an archive produced by [`catalog_bytes`].
pub(crate) fn archived_catalog_hash(bytes: &[u8]) -> Result<String> {
    Ok(digest(&shards::archive_root(bytes)?))
}

pub(crate) fn export_locked_repository(
    publisher: &crate::image::Publisher,
    hash: &str,
    packages: &[&Published],
    blobs: &BTreeMap<&str, &Node>,
    destination: &Path,
) -> Result<()> {
    let catalog = read_catalog(&publisher.repo, hash, &publisher.name, &publisher.key)?;
    fs::create_dir(destination)?;
    prepare_objects(destination)?;
    for package in packages {
        catalog.check_installed(package)?;
    }
    // Later plans can read any manifest of the pinned catalog, but only the
    // locked selection needs attestations and payloads to provision this image.
    let manifests = catalog
        .entries
        .values()
        .map(CatalogEntry::manifest_digest)
        .collect::<BTreeSet<_>>();
    let attestations = packages
        .iter()
        .filter_map(|package| catalog.attestations.get(&package.manifest.id()))
        .collect::<BTreeSet<_>>();
    let mut objects = vec![
        ("manifests", manifests, MANIFEST_LIMIT),
        (
            "attestations",
            attestations.into_iter().map(String::as_str).collect(),
            ATTESTATION_LIMIT,
        ),
    ];
    let metadata = catalog_objects(&publisher.repo, hash)?;
    for kind in [SHARD_OBJECTS, FILE_INDEX_OBJECTS] {
        let limit = if kind == SHARD_OBJECTS {
            SHARD_LIMIT
        } else {
            FILE_INDEX_LIMIT
        };
        let hashes = metadata
            .iter()
            .filter(|(object, _, _)| *object == kind)
            .map(|(_, hash, _)| hash.as_str())
            .collect();
        objects.push((kind, hashes, limit));
    }
    for (kind, hashes, limit) in objects {
        for digest in hashes {
            let content = read_object(&publisher.repo, kind, digest, limit)?;
            write_atomic(&destination.join(kind).join(digest), &content, true)?;
        }
    }
    for (&digest, node) in blobs {
        let mut file = File::create_new(destination.join("blobs").join(digest))?;
        stage_blob(&publisher.repo, node, &mut file)?;
        file.sync_all()?;
    }
    File::open(destination.join("blobs"))?.sync_all()?;
    let root = read_object(&publisher.repo, "catalogs", hash, CATALOG_ROOT_LIMIT)?;
    write_atomic(&destination.join("catalogs").join(hash), &root, true)?;
    Ok(())
}

/// Verify a signed root and its root-level policy. Shards load separately.
fn decode_root(
    bytes: &[u8],
    hash: &str,
    publisher: &str,
    key: &[u8],
    freshness: Freshness,
) -> Result<Catalog> {
    let opened = shards::open(bytes)?;
    let root = shards::Root::decode(&opened.payload)?;
    ensure!(
        root.publisher == publisher,
        "unexpected publisher {}",
        root.publisher
    );
    let verified = trust::verify(publisher, &root.trust, key, freshness)
        .with_context(|| format!("checking the trust root of catalog {hash}"))?;
    opened.verify(&verified, hash)?;
    let catalog = Catalog {
        publisher: root.publisher,
        entries: BTreeMap::new(),
        file_index: root.file_index,
        attestations: BTreeMap::new(),
        attestations_required: root.attestations_required,
        toolchains_verified: root.toolchains_verified,
        build_inputs: BTreeMap::new(),
        source_archives_required: root.source_archives_required,
        withdrawals: BTreeMap::new(),
        retired: BTreeMap::new(),
        pruned_through: root.pruned_through,
        sequence: root.sequence,
        profiles_required: root.profiles_required,
        obsoletions: BTreeMap::new(),
        renames: root.renames,
        trust: root.trust,
        trust_keys: verified.keys(),
        trust_genesis: verified.genesis().to_owned(),
        release_profiles: root.release_profiles,
        layout: Layout {
            committed: root.shards,
            loaded: BTreeMap::new(),
        },
    };
    ensure!(
        catalog.release_profiles.is_empty() || catalog.profiles_required,
        "signed composition profiles require the publishing gate"
    );
    ensure!(
        !catalog.source_archives_required || catalog.attestations_required,
        "source archive protection requires signed build evidence"
    );
    ensure!(
        !catalog.toolchains_verified || catalog.attestations_required,
        "toolchain verification requires signed build evidence"
    );
    catalog.validate_profile_build_trust()?;
    for (name, profile) in &catalog.release_profiles {
        validate_name(name).context("invalid signed profile name")?;
        validate_publisher(&profile.architecture).context("invalid signed profile architecture")?;
        ensure!(
            !(profile.elf_symbol_versions || profile.elf_require_declarations)
                || !profile.elf_library_dirs.is_empty(),
            "ELF checking policy requires library directories"
        );
        if !profile.elf_library_dirs.is_empty() {
            publish_policy::validate_elf_dirs(&profile.architecture, &profile.elf_library_dirs)?;
        }
        crate::model::validate_policy(&profile.facets, &BTreeMap::new(), &profile.variants)
            .context("invalid signed profile facets or variants")?;
        for facet in &profile.facet_overrides {
            crate::model::validate_facet_pattern(facet)
                .context("invalid signed profile facet override")?;
        }
        let mut publishers = BTreeSet::from([publisher]);
        for pin in &profile.companions {
            validate_publisher(&pin.publisher)?;
            validate_digest(&pin.sha256)?;
            let lineage = pin
                .lineage
                .as_ref()
                .context("missing signed companion lineage")?;
            validate_digest(&lineage.trust_root_sha256)?;
            ensure!(lineage.sequence > 0, "invalid signed companion sequence");
            ensure!(
                publishers.insert(pin.publisher.as_str()),
                "duplicate publisher {} in signed profile {name}",
                pin.publisher
            );
        }
    }
    Ok(catalog)
}

pub fn read_attestation(repo: &Path, publisher: &str, key: &Path, id: &str) -> Result<Vec<u8>> {
    let (catalog, _) = read_current_catalog(repo, publisher, &read_trust_root(key)?)?;
    let hash = catalog
        .package_record(id)
        .and_then(|(_, attestation)| attestation)
        .with_context(|| format!("no build attestation for {id}"))?;
    read_object(repo, "attestations", hash, ATTESTATION_LIMIT)
}

pub fn snapshot(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    pinned: Option<&str>,
) -> Result<Snapshot> {
    let (catalog, hash) = read_pinned_or_current(repo, publisher, key, pinned)?;
    catalog_snapshot(repo, catalog, hash)
}

fn read_pinned_or_current(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    pinned: Option<&str>,
) -> Result<(Catalog, String)> {
    match pinned {
        Some(hash) => Ok((read_catalog(repo, hash, publisher, key)?, hash.to_owned())),
        None => read_current_catalog(repo, publisher, key),
    }
}

/// Read every manifest of a catalog. Planning uses [`candidates`] instead.
fn catalog_snapshot(repo: &Path, mut catalog: Catalog, hash: String) -> Result<Snapshot> {
    let rename_targets = catalog.rename_targets()?;
    let metadata = catalog.metadata_objects();
    let builds = catalog.take_build_evidence(&hash)?;
    let mut packages = Vec::with_capacity(catalog.entries.len());
    for (id, entry) in catalog.entries {
        packages.push(read_package(repo, &catalog.publisher, &id, &entry)?);
    }
    Ok(Snapshot {
        sha256: hash,
        packages,
        builds,
        build_inputs: catalog.build_inputs,
        metadata,
        obsoletions: catalog.obsoletions,
        rename_targets,
    })
}

/// A signed catalog prepared for planning: candidates carry their signed
/// dependency summaries, and manifests load only for selected packages.
pub(crate) struct CandidateSnapshot {
    pub sha256: String,
    pub packages: Vec<Candidate>,
    pub(crate) builds: BuildEvidence,
    withdrawn: BTreeSet<String>,
    pub(crate) obsoletions: BTreeMap<String, Version>,
    pub(crate) rename_targets: BTreeMap<String, String>,
}

impl CandidateSnapshot {
    pub(crate) fn is_withdrawn(&self, id: &str) -> bool {
        self.withdrawn.contains(id)
    }
}

pub(crate) fn candidates(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    pinned: Option<&str>,
) -> Result<CandidateSnapshot> {
    let (catalog, hash) = read_pinned_or_current(repo, publisher, key, pinned)?;
    candidate_snapshot(repo, catalog, hash)
}

fn candidate_snapshot(
    repo: &Path,
    mut catalog: Catalog,
    hash: String,
) -> Result<CandidateSnapshot> {
    let rename_targets = catalog.rename_targets()?;
    let builds = catalog.take_build_evidence(&hash)?;
    let origin = Arc::new(repo.to_owned());
    let publisher = catalog.publisher.as_str();
    let packages = catalog
        .entries
        .into_iter()
        .map(|(id, entry)| Candidate::new(&origin, publisher, &id, entry))
        .collect::<Result<_>>()?;
    Ok(CandidateSnapshot {
        sha256: hash,
        packages,
        builds,
        withdrawn: catalog
            .withdrawals
            .into_keys()
            .chain(catalog.retired.into_keys())
            .collect(),
        obsoletions: catalog.obsoletions,
        rename_targets,
    })
}

/// What the resolver reads about a package version, whether from a signed
/// catalog summary or from a manifest already in hand.
pub(crate) trait PackageInfo {
    fn sha256(&self) -> &str;
    fn publisher(&self) -> &str;
    fn name(&self) -> &str;
    fn version(&self) -> &Version;
    fn architecture(&self) -> Option<&str>;
    fn dependencies(&self) -> &[Dependency];
    fn provides(&self) -> &BTreeMap<String, Option<Version>>;
    /// The verified manifest. Catalog candidates read it on first use.
    fn load(&self) -> Result<Published>;

    fn id(&self) -> String {
        format!(
            "cps://{}/{}@{}",
            self.publisher(),
            self.name(),
            self.version()
        )
    }

    fn supports_architecture(&self, architecture: &str) -> bool {
        self.architecture()
            .is_none_or(|target| target == architecture)
    }

    fn check_architecture(&self, architecture: &str) -> Result<()> {
        if let Some(target) = self.architecture() {
            ensure!(
                target == architecture,
                "{} targets architecture {target}, expected {architecture}",
                self.id()
            );
        }
        Ok(())
    }

    fn provides_capability(&self, name: &str, constraint: &VersionConstraint) -> bool {
        self.provides()
            .get(name)
            .is_some_and(|version| constraint.matches_provided(version.as_ref()))
    }
}

impl PackageInfo for Published {
    fn sha256(&self) -> &str {
        &self.sha256
    }
    fn supports_architecture(&self, architecture: &str) -> bool {
        self.manifest.supports_architecture(architecture)
    }
    fn publisher(&self) -> &str {
        &self.manifest.publisher
    }
    fn name(&self) -> &str {
        &self.manifest.name
    }
    fn version(&self) -> &Version {
        &self.manifest.version
    }
    fn architecture(&self) -> Option<&str> {
        self.manifest.architecture.as_deref()
    }
    fn dependencies(&self) -> &[Dependency] {
        &self.manifest.dependency
    }
    fn provides(&self) -> &BTreeMap<String, Option<Version>> {
        &self.manifest.provides
    }
    fn load(&self) -> Result<Published> {
        Ok(self.clone())
    }
}

impl<T: PackageInfo + ?Sized> PackageInfo for &T {
    fn sha256(&self) -> &str {
        (**self).sha256()
    }
    fn publisher(&self) -> &str {
        (**self).publisher()
    }
    fn name(&self) -> &str {
        (**self).name()
    }
    fn version(&self) -> &Version {
        (**self).version()
    }
    fn architecture(&self) -> Option<&str> {
        (**self).architecture()
    }
    fn dependencies(&self) -> &[Dependency] {
        (**self).dependencies()
    }
    fn provides(&self) -> &BTreeMap<String, Option<Version>> {
        (**self).provides()
    }
    fn load(&self) -> Result<Published> {
        (**self).load()
    }
}

/// A package version offered by a signed catalog.
#[derive(Clone)]
pub(crate) struct Candidate {
    pub sha256: String,
    pub manifest: ManifestSummary,
    repo: Arc<PathBuf>,
    loaded: Arc<OnceLock<Published>>,
}

/// The identity of a catalog candidate and its signed catalog record.
#[derive(Clone)]
pub(crate) struct ManifestSummary {
    pub publisher: String,
    pub name: String,
    pub version: Version,
    entry: CatalogEntry,
}

impl ManifestSummary {
    pub(crate) fn id(&self) -> String {
        format!("cps://{}/{}@{}", self.publisher, self.name, self.version)
    }
}

impl Candidate {
    fn new(repo: &Arc<PathBuf>, publisher: &str, id: &str, entry: CatalogEntry) -> Result<Self> {
        let spec = PackageSpec::parse(id)?;
        let version = spec
            .version
            .with_context(|| format!("catalog entry {id} has no version"))?;
        Ok(Self {
            sha256: entry.manifest_digest().to_owned(),
            manifest: ManifestSummary {
                publisher: publisher.to_owned(),
                name: spec.name.to_owned(),
                version,
                entry,
            },
            repo: repo.clone(),
            loaded: Arc::default(),
        })
    }
}

impl PackageInfo for Candidate {
    fn sha256(&self) -> &str {
        &self.sha256
    }
    fn publisher(&self) -> &str {
        &self.manifest.publisher
    }
    fn name(&self) -> &str {
        &self.manifest.name
    }
    fn version(&self) -> &Version {
        &self.manifest.version
    }
    fn architecture(&self) -> Option<&str> {
        self.manifest.entry.architecture()
    }
    fn dependencies(&self) -> &[Dependency] {
        self.manifest.entry.dependencies()
    }
    fn provides(&self) -> &BTreeMap<String, Option<Version>> {
        self.manifest.entry.provides()
    }
    fn load(&self) -> Result<Published> {
        if let Some(published) = self.loaded.get() {
            return Ok(published.clone());
        }
        let id = self.manifest.id();
        let published = read_package(&self.repo, self.publisher(), &id, &self.manifest.entry)
            .with_context(|| format!("reading manifest for {id}"))?;
        Ok(self.loaded.get_or_init(|| published).clone())
    }
}

pub(crate) struct CatalogIndex {
    pub entries: BTreeMap<String, CatalogEntry>,
    pub obsoletions: BTreeMap<String, Version>,
    pub checkpoint: CatalogCheckpoint,
    /// Per-shard file indexes, when the publisher signs one.
    pub file_index: Option<Vec<String>>,
}

pub(crate) fn catalog_index(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    pin: Option<&CatalogPin>,
) -> Result<CatalogIndex> {
    let (catalog, hash) = read_selected_catalog(repo, publisher, key, pin)?;
    let file_index = catalog.file_index.then(|| catalog.file_indexes());
    Ok(CatalogIndex {
        entries: catalog.entries,
        obsoletions: catalog.obsoletions,
        file_index,
        checkpoint: CatalogCheckpoint {
            catalog: hash,
            sequence: catalog.sequence,
            trust: catalog.trust,
        },
    })
}

pub(crate) fn read_selected_catalog(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    pin: Option<&CatalogPin>,
) -> Result<(Catalog, String)> {
    if let Some(pin) = pin {
        return Ok((pin.read(repo, publisher, key)?, pin.sha256.clone()));
    }
    read_current_catalog(repo, publisher, key)
}

pub(crate) fn release_profile(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    pin: Option<&str>,
    name: &str,
) -> Result<(CatalogCheckpoint, ReleaseProfile)> {
    let (mut catalog, hash) = read_pinned_or_current(repo, publisher, key, pin)?;
    let profile = take_profile(&mut catalog, &hash, name)?;
    Ok((
        CatalogCheckpoint {
            catalog: hash,
            sequence: catalog.sequence,
            trust: catalog.trust,
        },
        profile,
    ))
}

/// Verify a self-contained catalog archive from [`catalog_bytes`].
pub(crate) fn decode_archived_catalog(
    bytes: &[u8],
    hash: &str,
    publisher: &str,
    key: &[u8],
) -> Result<Catalog> {
    let (root, parts) = shards::unpack_archive(bytes, CATALOG_LIMIT)?;
    ensure!(digest(&root) == hash, "archived catalog digest mismatch");
    let mut catalog = decode_root(&root, hash, publisher, key, Freshness::Pinned)?;
    let committed: Vec<_> = catalog.layout.committed.clone().into_iter().collect();
    ensure!(
        committed.len() == parts.len(),
        "archived catalog has the wrong number of shards"
    );
    for ((prefix, shard), bytes) in committed.into_iter().zip(parts) {
        ensure!(
            digest(&bytes) == shard,
            "archived catalog shard digest mismatch"
        );
        catalog.load_shards([prefix], |hash| shards::decode_shard(&bytes, hash))?;
    }
    catalog.validate_complete()?;
    Ok(catalog)
}

pub(crate) fn take_profile(
    catalog: &mut Catalog,
    hash: &str,
    name: &str,
) -> Result<ReleaseProfile> {
    catalog.release_profiles.remove(name).with_context(|| {
        format!(
            "catalog {hash} for {} has no signed release profile {name}",
            catalog.publisher
        )
    })
}

pub fn list_obsoletions(repo: &Path, publisher: &str, key: &Path) -> Result<Vec<String>> {
    let index = catalog_index(repo, publisher, &read_trust_root(key)?, None)?;
    Ok(index
        .obsoletions
        .into_iter()
        .map(|(name, version)| format!("cps://{publisher}/{name}@{version}"))
        .collect())
}

pub(crate) fn read_package(
    repo: &Path,
    publisher: &str,
    id: &str,
    entry: &CatalogEntry,
) -> Result<Published> {
    let package = load_manifest(repo, entry.manifest_digest().to_owned())?;
    ensure!(
        package.manifest.id() == id && package.manifest.publisher == publisher,
        "manifest identity mismatch for {id}"
    );
    entry.check_manifest(&package.manifest)?;
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

/// Store a content-addressed publication object. Objects are written without
/// a per-object fsync: [`sync_objects`] makes a publication's objects durable
/// at once, before the signed root that references them.
fn store(repo: &Path, kind: &str, bytes: &[u8]) -> Result<String> {
    let hash = digest(bytes);
    let parent = repo.join(kind);
    if parent.join(&hash).try_exists()? {
        let existing = Root::open(&parent)
            .and_then(|parent| parent.file(&hash))
            .and_then(|file| read_limited(file, bytes.len() as u64));
        if existing.is_ok_and(|existing| existing == bytes) {
            return Ok(hash);
        }
        // An object that does not match its digest name is damaged, for
        // example by a crash before a sync. These bytes are the correct ones.
    }
    let mut file = NamedTempFile::new_in(&parent)?;
    file.write_all(bytes)?;
    file.persist(parent.join(&hash))?;
    Ok(hash)
}

/// Make every object written so far durable, with one filesystem sync
/// instead of an fsync per object.
fn sync_objects(repo: &Path) -> Result<()> {
    rustix::fs::syncfs(File::open(repo)?)
        .with_context(|| format!("syncing repository {}", repo.display()))
}

fn lock_repo(repo: &Path) -> Result<File> {
    fs::create_dir_all(repo)?;
    Root::open(repo)?;
    lock_file(&repo.join("publish.lock"))
}

fn lock_file(path: &Path) -> Result<File> {
    let lock = File::from(open(
        path,
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
    for dir in [
        "catalogs",
        SHARD_OBJECTS,
        "manifests",
        "blobs",
        "attestations",
        FILE_INDEX_OBJECTS,
        rebuild::OBJECTS,
    ] {
        if !path_present(&repo.join(dir))? {
            fs::create_dir(repo.join(dir))?;
        }
        root.dir(dir)?;
    }
    File::open(repo)?.sync_all()?;
    Ok(())
}

pub fn publish(
    repo: &Path,
    proto: &Path,
    recipe: &Path,
    key: &(impl KeySpecs + ?Sized),
) -> Result<String> {
    let mut ids = publish_batch(repo, proto, &[recipe.to_owned()], key)?;
    Ok(ids.remove(0))
}

/// Publish many manifests from one proto tree in a single signed catalog update.
pub fn publish_batch(
    repo: &Path,
    proto: &Path,
    recipes: &[PathBuf],
    key: &(impl KeySpecs + ?Sized),
) -> Result<Vec<String>> {
    ensure!(
        !recipes.is_empty(),
        "publish requires at least one manifest"
    );
    let key = Publication::open(repo, key)?;
    let manifests = recipes
        .iter()
        .map(|recipe| {
            let manifest_bytes = read_limited(File::open(recipe)?, MANIFEST_LIMIT)?;
            let manifest = parse_manifest(std::str::from_utf8(&manifest_bytes)?)
                .with_context(|| format!("parsing {}", recipe.display()))?;
            manifest.validate(false)?;
            Ok((manifest, None))
        })
        .collect::<Result<Vec<_>>>()?;
    publish_manifests(repo, &Root::open(proto)?, manifests, &key, None)
}

/// Publish checked manifests whose payloads come from an opened proto tree.
pub(crate) fn publish_from_root(
    repo: &Path,
    proto: &Root,
    manifests: Vec<Manifest>,
    key: &(impl KeySpecs + ?Sized),
) -> Result<Vec<String>> {
    let key = Publication::open(repo, key)?;
    let manifests = manifests
        .into_iter()
        .map(|manifest| (manifest, None))
        .collect();
    publish_manifests(repo, proto, manifests, &key, None)
}

fn exact_publisher(id: &str) -> Result<&str> {
    let spec = PackageSpec::parse(id)?;
    spec.publisher
        .filter(|_| spec.version.is_some())
        .context("expected a full package ID with publisher and version")
}

pub fn withdraw_package(repo: &Path, key: &(impl KeySpecs + ?Sized), id: &str) -> Result<()> {
    let publisher = exact_publisher(id)?;
    let key = Publication::open(repo, key)?;
    Root::open(repo)?;
    let _lock = lock_repo(repo)?;
    ensure!(
        !path_present(&repo.join(".tuf"))?,
        "cannot withdraw from a TUF-managed repository cache"
    );
    let current = read_current(repo)?;
    let mut catalog = publication_root(repo, publisher, &key, Some(&current))?;
    if path_present(&repo.join(PUBLISH_POLICY))? {
        catalog.load_all(repo)?;
    } else {
        catalog.load_names(repo, [PackageSpec::parse(id)?.name])?;
    }
    if catalog.withdraw(id)? {
        let policy = read_publish_policy(repo, &catalog, false)?;
        check_profiles(
            repo,
            &mut catalog,
            Some(&current),
            policy.as_ref(),
            key.anchor(),
        )?;
        write_catalog(repo, catalog, &key)?;
    }
    Ok(())
}

/// Publish a successor of the exact current catalog that carries the
/// repository's newest trust root history, signed by its catalog keys. After
/// a trust root rotation this is how clients learn of revoked keys.
pub fn resign_catalog(
    repo: &Path,
    publisher: &str,
    key: &(impl KeySpecs + ?Sized),
    base: &str,
) -> Result<String> {
    validate_publisher(publisher)?;
    validate_digest(base)?;
    let key = Publication::open(repo, key)?;
    let _lock = lock_repo(repo)?;
    let current = publication_base(repo)?.context("repository has no catalog to re-sign")?;
    ensure!(
        current == base,
        "re-signing base is stale: expected {base}, current is {current}"
    );
    // Only the root changes: every shard and file index is reused.
    let catalog = publication_root(repo, publisher, &key, Some(&current))?;
    write_catalog_document(repo, catalog, &key, ReleaseMode::Publish)
}

/// Lock the repository and read its trust root history.
fn lock_trust(repo: &Path) -> Result<(File, Option<trust::Store>)> {
    let lock = lock_repo(repo)?;
    Ok((lock, trust::Store::read(repo)?))
}

/// Create a repository's first trust root with one key as its only root key
/// and its only catalog key: a 1-of-1 setup. Returns the trust root's path.
pub fn init_trust(
    repo: &Path,
    publisher: &str,
    key: &Path,
    expires: Option<&str>,
) -> Result<PathBuf> {
    validate_publisher(publisher)?;
    let signers = Signers::open(key)?;
    let keys: BTreeSet<_> = signers.public_keys().into_iter().map(trust::Key).collect();
    let (_lock, store) = lock_trust(repo)?;
    ensure!(
        store.is_none(),
        "repository {} already has a trust root",
        repo.display()
    );
    let root = trust::draft_with(
        None,
        Some(publisher),
        Some(keys.clone()),
        None,
        Some(keys),
        None,
        expires,
    )?;
    let signed = trust::combine(None, &root.encode()?, Vec::new(), Some(&signers))?;
    trust::store(repo, &signed)
}

/// The unsigned payload of the repository's next trust root version.
pub fn draft_trust(repo: &Path, change: &TrustDraft<'_>) -> Result<Vec<u8>> {
    let store = trust::Store::read(repo)?;
    trust::draft(store.as_ref().map(|store| store.verified.current()), change)?.encode()
}

/// Detached signatures over a trust root draft, one JSON object per line.
pub fn sign_trust(draft: &Path, keys: &(impl KeySpecs + ?Sized)) -> Result<Vec<u8>> {
    trust::sign_detached(&trust::read_draft(draft)?, &Signers::open(keys)?)
}

/// Combine a draft with detached signatures and signing keys, verify it as
/// the repository's next trust root version, and store it.
pub fn commit_trust(
    repo: &Path,
    draft: &Path,
    signatures: &[PathBuf],
    keys: &[PathBuf],
) -> Result<PathBuf> {
    let payload = trust::read_draft(draft)?;
    let detached = signatures
        .iter()
        .map(|path| trust::read_detached(path))
        .collect::<Result<Vec<_>>>()?
        .concat();
    let signers = (!keys.is_empty())
        .then(|| Signers::open(keys))
        .transpose()?;
    let (_lock, store) = lock_trust(repo)?;
    let signed = trust::combine(
        store.as_ref().map(|store| store.verified.current()),
        &payload,
        detached,
        signers.as_ref(),
    )?;
    trust::store(repo, &signed)
}

/// Draft, sign, and store the next trust root version in one step.
pub fn rotate_trust(
    repo: &Path,
    change: &TrustDraft<'_>,
    keys: &(impl KeySpecs + ?Sized),
) -> Result<PathBuf> {
    let signers = Signers::open(keys)?;
    let (_lock, store) = lock_trust(repo)?;
    let store = store.with_context(|| {
        format!(
            "repository {} has no trust root; create one with `cps repo trust init`",
            repo.display()
        )
    })?;
    let previous = store.verified.current();
    let payload = trust::draft(Some(previous), change)?.encode()?;
    let signed = trust::combine(Some(previous), &payload, Vec::new(), Some(&signers))?;
    trust::store(repo, &signed)
}

/// The path of a stored trust root version in a repository.
pub(crate) fn trust_root_path(repo: &Path, version: u64) -> PathBuf {
    trust::Store::path(repo, version)
}

/// Re-sign the newest trust root version with a later expiry when it expires
/// within `days`. The key must meet both root thresholds on its own.
pub(crate) fn renew_trust(repo: &Path, key: &Path, days: i64) -> Result<bool> {
    let store = trust::Store::read(repo)?.context("repository has no trust root")?;
    let soon = jiff::Timestamp::now().checked_add(jiff::Span::new().hours(24 * days))?;
    if store.verified.current().expires() > soon {
        return Ok(false);
    }
    rotate_trust(repo, &TrustDraft::default(), key)?;
    Ok(true)
}

/// Describe a trust root file, or a repository's newest trust root version.
pub fn show_trust(path: &Path) -> Result<String> {
    trust::describe(path)
}

pub fn list_withdrawals(
    repo: &Path,
    publisher: &str,
    key: &Path,
) -> Result<BTreeMap<String, String>> {
    let (catalog, _) = read_current_catalog(repo, publisher, &read_trust_root(key)?)?;
    Ok(catalog
        .withdrawals
        .into_iter()
        .map(|(id, withdrawal)| (id, withdrawal.manifest.manifest_digest().to_owned()))
        .collect())
}

pub fn publish_bundle(
    repo: &Path,
    bundle: &Path,
    key: &(impl KeySpecs + ?Sized),
    builder_key: &Path,
    reviewed_recipe: &str,
) -> Result<Vec<String>> {
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
    key: &(impl KeySpecs + ?Sized),
    builder_key: &Path,
    reviewed_recipe: &str,
    witnesses: &[BuildWitness],
) -> Result<Vec<String>> {
    publish_witnessed_bundle_with_sources(
        repo,
        bundle,
        key,
        builder_key,
        reviewed_recipe,
        witnesses,
        None,
    )
}

pub fn publish_witnessed_bundle_with_sources(
    repo: &Path,
    bundle: &Path,
    key: &(impl KeySpecs + ?Sized),
    builder_key: &Path,
    reviewed_recipe: &str,
    witnesses: &[BuildWitness],
    source_cache: Option<&Path>,
) -> Result<Vec<String>> {
    let key = Publication::open(repo, key)?;
    let (proto, manifests, verified) =
        verify_publication(bundle, builder_key, reviewed_recipe, witnesses, &key.keys())?;
    let source_cache = source_cache.map(Root::open).transpose()?;
    // Every output of the build is published in one signed catalog update.
    publish_manifests(
        repo,
        &proto,
        manifests
            .into_iter()
            .map(|manifest| (manifest, Some(&verified)))
            .collect(),
        &key,
        source_cache.as_ref(),
    )
}

fn verify_publication(
    bundle: &Path,
    builder_key: &Path,
    reviewed_recipe: &str,
    witnesses: &[BuildWitness],
    publisher_keys: &BTreeSet<PublicKey>,
) -> Result<(Root, Vec<Manifest>, VerifiedBuild)> {
    ensure!(
        witnesses.len() < MAX_BUILDERS,
        "at most {MAX_BUILDERS} build signers are supported"
    );
    let distinct = |builder: &[u8]| {
        ensure!(
            !publisher_keys.iter().any(|key| key.as_slice() == builder),
            "builder and publisher must use distinct keys"
        );
        Ok(())
    };
    let bundle = Root::open(bundle)?;
    let builder = read_key(builder_key)?;
    distinct(&builder)?;
    let (manifests, mut verified) = verify_bundle(&bundle, &builder, reviewed_recipe)?;
    for witness in witnesses {
        let builder = read_key(&witness.key)?;
        distinct(&builder)?;
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
    Ok((bundle.subdir("proto")?, manifests, verified))
}

pub fn publish_release(
    repo: &Path,
    key: &(impl KeySpecs + ?Sized),
    document: &Path,
) -> Result<String> {
    publish_release_with_sources(repo, key, document, None)
}

pub fn publish_release_with_sources(
    repo: &Path,
    key: &(impl KeySpecs + ?Sized),
    document: &Path,
    source_cache: Option<&Path>,
) -> Result<String> {
    publish_release_with_mode(repo, key, document, source_cache, ReleaseMode::Publish)
}

#[derive(Clone, Copy)]
pub enum ReleaseMode<'a> {
    Publish,
    Prepare,
    RequireCandidate(&'a str),
}

impl ReleaseMode<'_> {
    fn check(self, hash: &str) -> Result<()> {
        if let Self::RequireCandidate(expected) = self {
            validate_digest(expected)?;
            ensure!(
                expected == hash,
                "release candidate differs from tested catalog {expected}: got {hash}"
            );
        }
        Ok(())
    }
}

pub fn publish_release_with_mode(
    repo: &Path,
    key: &(impl KeySpecs + ?Sized),
    document: &Path,
    source_cache: Option<&Path>,
    mode: ReleaseMode<'_>,
) -> Result<String> {
    let bytes = read_limited(File::open(document)?, PUBLISH_RELEASE_LIMIT)?;
    let release = publish_release::parse(std::str::from_utf8(&bytes)?)
        .with_context(|| format!("reading release {}", document.display()))?;
    let parent = document.parent().unwrap_or_else(|| Path::new("."));
    let key = Publication::open(repo, key)?;
    let _lock = lock_repo(repo)?;
    let current = publication_base(repo)?;
    ensure!(
        release.base == current,
        "release base is stale: expected {}, current is {}",
        release.base.as_deref().unwrap_or("empty"),
        current.as_deref().unwrap_or("empty"),
    );
    let mut catalog = publication_catalog(repo, &release.publisher, &key, current.as_deref())?;
    let publishes_bundles = release
        .changes
        .iter()
        .any(|change| matches!(change, publish_release::Change::Bundle { .. }));
    ensure!(
        source_cache.is_none() || publishes_bundles,
        "--source-cache requires a release with build bundles"
    );
    let policy = read_publish_policy(repo, &catalog, publishes_bundles)?;
    let source_cache = source_cache.map(Root::open).transpose()?;
    ensure!(
        !release.refresh_profiles
            || policy
                .as_ref()
                .is_some_and(|policy| !policy.profiles.is_empty()),
        "refresh-profiles requires release composition profiles in the publish policy"
    );
    if publishes_bundles {
        prepare_objects(repo)?;
    }
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
                let (proto, manifests, verified) = verify_publication(
                    &parent.join(path),
                    &parent.join(builder_key),
                    &recipe,
                    &witnesses,
                    &key.keys(),
                )
                .with_context(|| format!("verifying bundle for {id}"))?;
                ensure!(
                    manifests.iter().any(|manifest| manifest.id() == id),
                    "bundle for {id} contains only {}",
                    manifests
                        .iter()
                        .map(Manifest::id)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                // A build's outputs share one attestation and are published together.
                for manifest in manifests {
                    let output = manifest.id();
                    changed |= stage_manifest(
                        repo,
                        &mut catalog,
                        &proto,
                        manifest,
                        Some(&verified),
                        policy.as_ref(),
                        source_cache.as_ref(),
                    )
                    .with_context(|| format!("staging {output}"))?;
                }
            }
            publish_release::Change::Withdraw(id) => changed |= catalog.withdraw(&id)?,
            publish_release::Change::Obsolete(id) => changed |= catalog.obsolete(&id)?,
            publish_release::Change::Rename { id, to } => changed |= catalog.rename(&id, to)?,
        }
    }
    if changed || release.refresh_profiles {
        catalog.rename_targets()?;
        let previous = (!changed).then(|| catalog.release_profiles.clone());
        check_profiles(
            repo,
            &mut catalog,
            current.as_deref(),
            policy.as_ref(),
            key.anchor(),
        )?;
        if previous.is_none_or(|profiles| profiles != catalog.release_profiles) {
            return save_catalog(repo, catalog, &key, mode);
        }
    }
    let hash = current.context("empty release has no published catalog")?;
    mode.check(&hash)?;
    Ok(hash)
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

/// The catalog signers and repository trust root history of one publication.
struct Publication {
    signers: Signers,
    trust: trust::Store,
    /// The version 1 trust root: the publisher's own pin for its catalogs.
    anchor: Vec<u8>,
}

impl Publication {
    /// Load the signing keys. They must be exactly the catalog threshold of
    /// catalog keys in the repository's newest trust root version.
    fn open(repo: &Path, keys: &(impl KeySpecs + ?Sized)) -> Result<Self> {
        let signers = Signers::open(keys)?;
        let trust = Self::read_trust(repo, &signers)?;
        Ok(Self {
            anchor: trust.genesis_bytes()?,
            signers,
            trust,
        })
    }

    fn read_trust(repo: &Path, signers: &Signers) -> Result<trust::Store> {
        let trust = trust::Store::read(repo)?.with_context(|| {
            format!(
                "repository {} has no publisher trust root; create one with `cps repo trust init`",
                repo.display()
            )
        })?;
        let current = trust.verified.current();
        let keys = signers.public_keys();
        ensure!(
            keys.is_subset(&current.catalog_keys()),
            "a signing key is not a catalog key of trust root version {}",
            current.version
        );
        ensure!(
            keys.len() == current.catalog_threshold,
            "trust root version {} requires exactly {} catalog signing keys, got {}",
            current.version,
            current.catalog_threshold,
            keys.len()
        );
        Ok(trust)
    }

    /// Re-read the trust root history while the caller holds the repository
    /// lock. Trust root changes take the same lock, so a key revoked after
    /// `open` cannot sign under the history it was loaded with.
    fn locked_trust(&self, repo: &Path) -> Result<trust::Store> {
        let trust = Self::read_trust(repo, &self.signers)?;
        ensure!(
            trust.genesis_bytes()? == self.anchor,
            "repository trust root changed while publishing"
        );
        Ok(trust)
    }

    fn anchor(&self) -> &[u8] {
        &self.anchor
    }

    /// Every key of every trust root version.
    fn keys(&self) -> BTreeSet<PublicKey> {
        self.trust.verified.keys()
    }
}

fn publication_catalog(
    repo: &Path,
    publisher: &str,
    key: &Publication,
    current: Option<&str>,
) -> Result<Catalog> {
    match current {
        Some(hash) => {
            read_catalog_with_freshness(repo, hash, publisher, key.anchor(), Freshness::Pinned)
        }
        None => Ok(Catalog::empty(publisher)),
    }
}

/// The signed root of the publication base, with no shards loaded yet.
fn publication_root(
    repo: &Path,
    publisher: &str,
    key: &Publication,
    current: Option<&str>,
) -> Result<Catalog> {
    match current {
        Some(hash) => read_root(repo, hash, publisher, key.anchor(), Freshness::Pinned),
        None => Ok(Catalog::empty(publisher)),
    }
}

fn publish_manifests(
    repo: &Path,
    proto: &Root,
    manifests: Vec<(Manifest, Option<&VerifiedBuild>)>,
    key: &Publication,
    source_cache: Option<&Root>,
) -> Result<Vec<String>> {
    let _lock = lock_repo(repo)?;
    let current = publication_base(repo)?;
    let publisher = &manifests
        .first()
        .context("no manifests to publish")?
        .0
        .publisher;
    let mut catalog = publication_root(repo, publisher, key, current.as_deref())?;
    // Plain publications touch only the shards of the published names. Build
    // evidence and publish policies may depend on every record.
    if path_present(&repo.join(PUBLISH_POLICY))?
        || manifests.iter().any(|(_, evidence)| evidence.is_some())
    {
        catalog.load_all(repo)?;
    } else {
        catalog.load_names(
            repo,
            manifests.iter().map(|(manifest, _)| manifest.name.as_str()),
        )?;
    }
    let policy = read_publish_policy(repo, &catalog, true)?;
    prepare_objects(repo)?;
    let mut ids = Vec::with_capacity(manifests.len());
    let mut changed = false;
    for (manifest, evidence) in manifests {
        let id = manifest.id();
        ensure!(
            !catalog.renames.contains_key(&manifest.name),
            "cannot publish {id} under a renamed package name"
        );
        changed |= stage_manifest(
            repo,
            &mut catalog,
            proto,
            manifest,
            evidence,
            policy.as_ref(),
            source_cache,
        )?;
        ids.push(id);
    }
    if changed {
        check_profiles(
            repo,
            &mut catalog,
            current.as_deref(),
            policy.as_ref(),
            key.anchor(),
        )?;
        write_catalog(repo, catalog, key)?;
    }
    Ok(ids)
}

fn stage_manifest(
    repo: &Path,
    catalog: &mut Catalog,
    proto: &Root,
    manifest: Manifest,
    evidence: Option<&VerifiedBuild>,
    policy: Option<&PublicationPolicy>,
    source_cache: Option<&Root>,
) -> Result<bool> {
    ensure!(
        source_cache.is_none() || policy.is_some_and(|policy| policy.archive_build_inputs),
        "--source-cache requires archive-build-inputs in the publish policy"
    );
    ensure!(
        manifest.publisher == catalog.publisher,
        "bundle belongs to a different publisher"
    );
    let id = manifest.id();
    ensure!(
        !catalog.is_withdrawn(&id),
        "{id} was withdrawn and cannot be republished"
    );
    if let Some(cutoff) = catalog.obsoletions.get(&manifest.name) {
        ensure!(
            manifest.version.semantic() > cutoff.semantic(),
            "{id} is obsolete through {cutoff}"
        );
    }
    if let Some(policy) = policy {
        policy.verify(&id, evidence)?;
        catalog.attestations_required = true;
        catalog.toolchains_verified |= policy.verify_toolchains;
    }
    if let Some(evidence) = evidence {
        for key in &catalog.trust_keys {
            let publisher = digest(key);
            ensure!(
                evidence.builders().all(|builder| builder != publisher),
                "a publisher trust root key cannot sign builds"
            );
        }
    }
    let tmpfiles: std::collections::BTreeSet<_> = manifest
        .systemd
        .iter()
        .filter(|effect| matches!(effect, SystemdAction::Tmpfiles(_)))
        .map(SystemdAction::path)
        .collect();
    let artifacts = manifest
        .payload_paths()
        .into_iter()
        .map(|path| {
            let input = proto
                .file(path)
                .with_context(|| format!("reading build output {path}"))?;
            let expected = evidence
                .map(|build| {
                    build
                        .artifacts
                        .get(path)
                        .context("missing verified build output")
                })
                .transpose()?;
            let file = store_blob(repo, input, tmpfiles.contains(path), expected)
                .with_context(|| format!("publishing {path}"))?;
            Ok((path.to_owned(), file))
        })
        .collect::<Result<_>>()?;
    // Generated ELF declarations come from the stored blobs, the bytes that
    // the catalog publishes.
    let blobs = Root::open(repo)?;
    let manifest = crate::generate::elf::published_manifest(manifest, &artifacts, |_, file| {
        blobs.file(&format!("blobs/{}", file.sha256))
    })?;
    let unchanged_manifest = if let Some(existing) = catalog.entries.get(&id) {
        let previous = read_package(repo, &catalog.publisher, &id, existing)?;
        ensure!(
            previous.manifest == manifest,
            "{id} was already published with different content"
        );
        true
    } else {
        let document = manifest_text(&manifest);
        ensure!(
            document.len() as u64 <= MANIFEST_LIMIT,
            "manifest exceeds {MANIFEST_LIMIT} bytes"
        );
        let sha256 = store(repo, "manifests", document.as_bytes())?;
        catalog
            .entries
            .insert(id.clone(), CatalogEntry::new(sha256, &manifest));
        false
    };
    if let Some(evidence) = evidence {
        ensure!(
            evidence
                .published
                .get(&id)
                .is_some_and(|published| catalog.entries[&id].manifest_digest() == published.sha256),
            "build attestation names a different published manifest for {id}"
        );
        let envelope = evidence.envelope()?;
        let hash = digest(&envelope);
        let added_attestation = if let Some(previous) = catalog.attestations.get(&id) {
            ensure!(
                previous == &hash,
                "{id} already has a different build attestation"
            );
            read_object(repo, "attestations", previous, ATTESTATION_LIMIT)?;
            false
        } else {
            let hash = store(repo, "attestations", &envelope)?;
            catalog.attestations.insert(id.clone(), hash);
            true
        };
        let added_inputs = if policy.is_some_and(|policy| policy.archive_build_inputs) {
            rebuild::archive(repo, catalog, &id, evidence)?
        } else {
            false
        };
        let require_archives = policy.is_some_and(|policy| policy.require_source_archives);
        let added_sources = if source_cache.is_some() || require_archives {
            source_archive::stage(repo, catalog, &id, &evidence.recipe, source_cache)?.0
        } else {
            false
        };
        let enabled_requirement = require_archives && !catalog.source_archives_required;
        catalog.source_archives_required |= require_archives;
        return Ok(!unchanged_manifest
            || added_attestation
            || added_inputs
            || added_sources
            || enabled_requirement);
    } else if unchanged_manifest {
        return Ok(false);
    }
    Ok(true)
}

type FileIndex = BTreeMap<String, BTreeSet<String>>;

const FILE_INDEX_FORMAT: u32 = 1;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileIndexDocument {
    format: u32,
    entries: FileIndex,
}

pub(crate) fn read_file_index(repo: &Path, hash: &str) -> Result<FileIndex> {
    let bytes = read_object(repo, FILE_INDEX_OBJECTS, hash, FILE_INDEX_LIMIT)?;
    let document: FileIndexDocument = serde_json::from_slice(&bytes)?;
    ensure!(
        document.format == FILE_INDEX_FORMAT,
        "unsupported file index format {}",
        document.format
    );
    Ok(document.entries)
}

/// The package IDs whose manifests have a file with this basename,
/// according to the signed per-shard file indexes.
pub(crate) fn search_file_index(
    repo: &Path,
    indexes: &[String],
    basename: &str,
) -> Result<BTreeSet<String>> {
    let mut found = BTreeSet::new();
    for hash in indexes {
        if let Some(ids) = read_file_index(repo, hash)?.remove(basename) {
            found.extend(ids);
        }
    }
    Ok(found)
}

/// Enable signed per-shard file lookup indexes for this publisher's catalogs.
pub fn enable_file_index(
    repo: &Path,
    publisher: &str,
    key: &(impl KeySpecs + ?Sized),
) -> Result<String> {
    validate_publisher(publisher)?;
    let key = Publication::open(repo, key)?;
    let _lock = lock_repo(repo)?;
    let hash = publication_base(repo)?.context("repository has no catalog to index")?;
    let mut catalog = publication_catalog(repo, publisher, &key, Some(&hash))?;
    if catalog.file_index {
        for index in catalog.file_indexes() {
            read_file_index(repo, &index)?;
        }
        return Ok(hash);
    }
    prepare_objects(repo)?;
    catalog.file_index = true;
    write_catalog_document(repo, catalog, &key, ReleaseMode::Publish)
}

/// Widen an admitted composition's rollout while retaining every other release field.
pub fn widen_rollout(
    repo: &Path,
    publisher: &str,
    key: &(impl KeySpecs + ?Sized),
    base: &str,
    profile: &str,
    percentage: u8,
) -> Result<String> {
    validate_publisher(publisher)?;
    validate_name(profile)?;
    validate_digest(base)?;
    ensure!(
        percentage <= 100,
        "rollout percentage must be between 0 and 100"
    );
    let key = Publication::open(repo, key)?;
    let _lock = lock_repo(repo)?;
    let current = publication_base(repo)?.context("repository has no release to widen")?;
    ensure!(
        current == base,
        "rollout base is stale: expected {base}, current is {current}"
    );
    // A rollout changes only the root: every shard and file index is reused.
    let mut catalog = publication_root(repo, publisher, &key, Some(&current))?;
    let profile = catalog
        .release_profiles
        .get_mut(profile)
        .context("unknown release profile")?;
    let previous = profile
        .rollout_percentage
        .context("profile has no phased rollout")?;
    ensure!(
        percentage > previous,
        "rollout must increase from {previous}%"
    );
    profile.rollout_percentage = Some(percentage);
    write_catalog_document(repo, catalog, &key, ReleaseMode::Publish)
}

/// Replace withdrawal records admitted through catalog sequence `through` with
/// signed tombstones, under the explicit retention cutoff recorded in the root.
///
/// Historical catalogs keep their full records, so pinned plans and profiles
/// that name them still resolve while their objects are kept. A tombstone keeps
/// the manifest digest: the ID cannot be republished, and installed copies
/// still verify against later catalogs.
pub fn prune_withdrawals(
    repo: &Path,
    publisher: &str,
    key: &(impl KeySpecs + ?Sized),
    base: &str,
    through: u64,
) -> Result<(String, usize)> {
    validate_publisher(publisher)?;
    validate_digest(base)?;
    let key = Publication::open(repo, key)?;
    let _lock = lock_repo(repo)?;
    let current = publication_base(repo)?.context("repository has no catalog to prune")?;
    ensure!(
        current == base,
        "pruning base is stale: expected {base}, current is {current}"
    );
    let mut catalog = publication_catalog(repo, publisher, &key, Some(&current))?;
    let pruned = catalog.prune_withdrawals(through)?;
    if pruned == 0 && catalog.pruned_through == through {
        let previous = publication_root(repo, publisher, &key, Some(&current))?;
        if previous.pruned_through == through {
            return Ok((current, 0));
        }
    }
    Ok((write_catalog(repo, catalog, &key)?, pruned))
}

/// Update one shard's file index from its previous index: only manifests
/// added to the shard are read.
fn index_shard(repo: &Path, previous: Option<&Shard>, shard: &Shard) -> Result<Option<String>> {
    let reusable = previous.and_then(|previous| {
        let index = previous.file_index.as_ref()?;
        Some((previous, index))
    });
    let (mut index, known) = match reusable {
        Some((previous, hash)) if previous.entries == shard.entries => {
            return Ok(Some(hash.clone()));
        }
        Some((previous, hash)) => {
            let mut index = read_file_index(repo, hash)?;
            index.retain(|_, ids| {
                ids.retain(|id| {
                    shard.entries.get(id) == previous.entries.get(id)
                        && shard.entries.contains_key(id)
                });
                !ids.is_empty()
            });
            (index, Some(&previous.entries))
        }
        None => (FileIndex::new(), None),
    };
    for (id, entry) in &shard.entries {
        if known.is_some_and(|known| known.get(id) == Some(entry)) {
            continue;
        }
        let package = read_package(repo, &shard.publisher, id, entry)?;
        for action in &package.manifest.action {
            let path = action.path();
            let basename = path.rsplit('/').next().unwrap_or(path);
            index
                .entry(basename.to_owned())
                .or_default()
                .insert(id.clone());
        }
    }
    let bytes = serde_json::to_vec(&FileIndexDocument {
        format: FILE_INDEX_FORMAT,
        entries: index,
    })?;
    ensure!(
        bytes.len() as u64 <= FILE_INDEX_LIMIT,
        "file index exceeds {FILE_INDEX_LIMIT} bytes"
    );
    Ok(Some(store(repo, FILE_INDEX_OBJECTS, &bytes)?))
}

fn write_catalog(repo: &Path, catalog: Catalog, key: &Publication) -> Result<String> {
    write_catalog_document(repo, catalog, key, ReleaseMode::Publish)
}

fn save_catalog(
    repo: &Path,
    catalog: Catalog,
    key: &Publication,
    mode: ReleaseMode<'_>,
) -> Result<String> {
    write_catalog_document(repo, catalog, key, mode)
}

/// Sign and store a catalog. The caller holds the repository lock.
fn write_catalog_document(
    repo: &Path,
    mut catalog: Catalog,
    key: &Publication,
    mode: ReleaseMode<'_>,
) -> Result<String> {
    catalog.rename_targets()?;
    catalog.check_source_archives()?;
    catalog.sequence = catalog.next_sequence()?.get();
    let trust = key.locked_trust(repo)?;
    let current = trust.verified.current();
    ensure!(
        current.publisher == catalog.publisher,
        "repository trust root belongs to publisher {}, not {}",
        current.publisher,
        catalog.publisher
    );
    current.check_unexpired()?;
    // The new catalog carries the repository's whole trust root history,
    // which must extend the history of the catalog it replaces.
    catalog.trust.check_successor(&trust.chain)?;
    catalog.trust_keys = trust.verified.keys();
    catalog.trust_genesis = trust.verified.genesis().to_owned();
    catalog.trust = trust.chain;
    catalog.validate_profile_build_trust()?;
    prepare_objects(repo)?;
    let root = catalog.commit(repo)?;
    sync_objects(repo)?;
    let signed = shards::seal(&serde_json::to_vec(&root)?, &key.signers)?;
    ensure!(
        signed.len() as u64 <= CATALOG_ROOT_LIMIT,
        "catalog root exceeds {CATALOG_ROOT_LIMIT} bytes"
    );
    let hash = digest(&signed);
    mode.check(&hash)?;
    write_atomic(&repo.join("catalogs").join(&hash), &signed, true)?;
    File::open(repo)?.sync_all()?;
    if !matches!(mode, ReleaseMode::Prepare) {
        write_atomic(&repo.join("current"), format!("{hash}\n").as_bytes(), false)?;
    }
    Ok(hash)
}

struct PublicationPolicy {
    approved: BTreeSet<String>,
    recipes: BTreeMap<String, publish_policy::RecipeApproval>,
    minimum_builders: usize,
    verify_toolchains: bool,
    archive_build_inputs: bool,
    require_source_archives: bool,
    profiles: BTreeMap<String, publish_policy::Profile>,
}

impl PublicationPolicy {
    fn verify(&self, id: &str, evidence: Option<&VerifiedBuild>) -> Result<()> {
        let build = evidence.context("publish policy requires an attested build bundle")?;
        let approval = self
            .recipes
            .get(id)
            .filter(|approval| approval.sha256 == build.recipe_sha256)
            .with_context(|| format!("build recipe is not approved for {id}"))?;
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
        if let Some(config) = &approval.toolchain_config {
            crate::build::verify_toolchain(config, &build.recipe, &build.toolchain)
                .with_context(|| format!("checking the build toolchain for {id}"))?;
        }
        Ok(())
    }
}

fn check_profiles(
    repo: &Path,
    catalog: &mut Catalog,
    current: Option<&str>,
    policy: Option<&PublicationPolicy>,
    key: &[u8],
) -> Result<()> {
    if let Some(policy) = policy.filter(|policy| !policy.profiles.is_empty()) {
        let release_profiles = profiles::check(repo, catalog, current, &policy.profiles, key)?;
        catalog.check_profile_guards(&release_profiles)?;
        catalog.profiles_required = true;
        catalog.release_profiles = release_profiles;
    }
    Ok(())
}

fn read_publish_policy(
    repo: &Path,
    catalog: &Catalog,
    publishes_bundles: bool,
) -> Result<Option<PublicationPolicy>> {
    if !path_present(&repo.join(PUBLISH_POLICY))? {
        ensure!(
            !catalog.profiles_required && (!publishes_bundles || !catalog.attestations_required),
            "protected repository is missing {PUBLISH_POLICY}"
        );
        return Ok(None);
    }
    ensure!(
        !publishes_bundles || catalog.attestations_required || catalog.entries.is_empty(),
        "configure publish policy before the repository's first publication"
    );
    let bytes = read_limited(
        Root::open(repo)?.file(PUBLISH_POLICY)?,
        PUBLISH_POLICY_LIMIT,
    )?;
    let policy = publish_policy::parse(std::str::from_utf8(&bytes)?, repo)
        .with_context(|| format!("reading {PUBLISH_POLICY}"))?;
    ensure!(
        policy.publisher == catalog.publisher,
        "publish policy names a different publisher"
    );
    ensure!(
        !catalog.profiles_required || !policy.profiles.is_empty(),
        "protected repository requires release composition profiles"
    );
    if publishes_bundles {
        ensure!(
            !catalog.source_archives_required || policy.require_source_archives,
            "protected repository requires source archives"
        );
        ensure!(
            catalog.build_inputs.is_empty() || policy.archive_build_inputs,
            "protected repository requires archiving rebuild inputs"
        );
        ensure!(
            !catalog.toolchains_verified || policy.verify_toolchains,
            "protected repository requires toolchain verification"
        );
        ensure!(
            !policy.verify_toolchains
                || catalog.toolchains_verified
                || (catalog.entries.is_empty() && catalog.withdrawals.is_empty()),
            "enable toolchain verification before the repository's first publication"
        );
    }
    let mut approved = BTreeSet::new();
    if publishes_bundles {
        for path in policy.builder_keys {
            let key = read_key(&repo.join(path))?;
            approved.insert(digest(&key));
        }
        ensure!(
            approved.len() >= policy.minimum_builders,
            "publish policy has fewer distinct builder keys than its threshold"
        );
    }
    Ok(Some(PublicationPolicy {
        approved,
        recipes: policy.recipes,
        minimum_builders: policy.minimum_builders,
        verify_toolchains: policy.verify_toolchains,
        archive_build_inputs: policy.archive_build_inputs,
        require_source_archives: policy.require_source_archives,
        profiles: policy.profiles,
    }))
}

/// Blobs this process has already checked under a repository, so a batch
/// that reuses a payload many times reads the stored copy once.
static VERIFIED_BLOBS: std::sync::LazyLock<std::sync::Mutex<BTreeSet<PathBuf>>> =
    std::sync::LazyLock::new(Default::default);

fn store_blob(
    repo: &Path,
    mut input: File,
    validate_tmpfiles: bool,
    expected: Option<&FileDigest>,
) -> Result<FileDigest> {
    let limit = expected.map_or(u64::MAX, |file| file.size.saturating_add(1));
    if let Some(file) = expected {
        ensure!(
            input.metadata()?.len() == file.size,
            "attested payload length mismatch"
        );
    }
    let mut hasher = sha2::Sha256::default();
    let size = io::copy(&mut (&mut input).take(limit), &mut hasher)?;
    let hash = hex::encode(sha2::Digest::finalize(hasher));
    ensure!(
        expected.is_none_or(|file| file.size == size && file.sha256 == hash),
        "attested payload digest mismatch"
    );
    if validate_tmpfiles {
        input.rewind()?;
        let mut bytes = Vec::new();
        (&mut input)
            .take(TMPFILES_LIMIT + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= TMPFILES_LIMIT && digest(&bytes) == hash,
            "tmpfiles payload exceeds {TMPFILES_LIMIT} bytes or changed while publishing"
        );
        systemd::validate_tmpfiles(&bytes)?;
    }
    let parent = repo.join("blobs");
    let path = parent.join(&hash);
    let intact = || -> Result<bool> {
        if VERIFIED_BLOBS
            .lock()
            .is_ok_and(|verified| verified.contains(&path))
        {
            return Ok(true);
        }
        let Ok(existing) = Root::open(repo)?.file(&format!("blobs/{hash}")) else {
            return Ok(false);
        };
        Ok(existing.metadata()?.len() == size
            && digest_reader(existing.take(size.saturating_add(1)))? == hash)
    };
    // A damaged blob at this digest is replaced with the verified payload.
    if !(path.try_exists()? && intact()?) {
        input.rewind()?;
        let mut temp = NamedTempFile::new_in(&parent)?;
        io::copy(&mut (&mut input).take(limit), &mut temp)?;
        temp.as_file_mut().rewind()?;
        // The proto file could change between the two reads.
        ensure!(
            digest_reader(temp.as_file_mut())? == hash,
            "payload changed while publishing"
        );
        temp.persist(&path)?;
    }
    if let Ok(mut verified) = VERIFIED_BLOBS.lock() {
        verified.insert(path);
    }
    Ok(FileDigest { sha256: hash, size })
}

pub fn stage_blob(repo: &Path, node: &Node, destination: &mut File) -> Result<()> {
    let Node::File { sha256, .. } = node else {
        anyhow::bail!("expected a file payload");
    };
    validate_digest(sha256)?;
    let source = Root::open(repo)?
        .file(&format!("blobs/{sha256}"))
        .with_context(|| format!("opening repository payload {sha256}"))?;
    copy_payload(&source, node, destination)
}

pub fn canonical_repo(repo: &Path) -> Result<PathBuf> {
    fs::canonicalize(repo).with_context(|| format!("opening repository {}", repo.display()))
}

#[cfg(test)]
pub(crate) use tests::sign_profile_with_missing_dependency;

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sign_profile_with_missing_dependency(
        repo: &Path,
        publisher: &str,
        key: &Path,
        profile: &str,
        dependency: &str,
    ) -> Result<String> {
        let signer = Publication::open(repo, key)?;
        let current = read_current(repo)?;
        let mut catalog = publication_catalog(repo, publisher, &signer, Some(&current))?;
        let composition = &mut catalog
            .release_profiles
            .get_mut(profile)
            .unwrap()
            .composition;
        *composition.get_mut(dependency).unwrap() = digest(b"missing signed profile dependency");
        write_catalog(repo, catalog, &signer)
    }

    /// A fresh repository with a 1-of-1 trust root, its publication signer,
    /// and the pinned trust root.
    fn test_publication(repo: &Path, publisher: &str) -> Result<(Publication, Vec<u8>)> {
        let key = repo.join("signing-key.der");
        fs::create_dir_all(repo)?;
        fs::write(
            &key,
            Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap(),
        )?;
        let path = init_trust(repo, publisher, &key, None)?;
        Ok((Publication::open(repo, &key)?, read_trust_root(&path)?))
    }

    #[test]
    fn publications_recheck_the_trust_root_under_the_repository_lock() -> Result<()> {
        let keys = trust::tests::Keys::new(2)?;
        let work = tempfile::tempdir()?;
        let repo = work.path().join("repo");
        init_trust(&repo, "canopa", &keys.private(0), None)?;
        // A publication loads its keys before it takes the repository lock.
        let stale = Publication::open(&repo, &keys.private(0))?;
        rotate_trust(
            &repo,
            &TrustDraft {
                catalog_keys: &[keys.public(1)],
                ..TrustDraft::default()
            },
            &keys.private(0),
        )?;
        let error = write_catalog(&repo, Catalog::empty("canopa"), &stale).unwrap_err();
        assert!(
            format!("{error:#}").contains("not a catalog key of trust root version 2"),
            "{error:#}"
        );
        assert!(!repo.join("current").exists());
        let hash = write_catalog(
            &repo,
            Catalog::empty("canopa"),
            &Publication::open(&repo, &keys.private(1))?,
        )?;
        let pinned = fs::read(trust_root_path(&repo, 1))?;
        let catalog = read_catalog(&repo, &hash, "canopa", &pinned)?;
        assert_eq!(catalog.trust.versions().len(), 2);
        Ok(())
    }

    #[test]
    fn catalogs_need_the_threshold_and_revoked_keys_only_verify_held_history() -> Result<()> {
        let keys = trust::tests::Keys::new(4)?;
        let work = tempfile::tempdir()?;
        let repo = work.path().join("repo");
        // Version 1 through the detached flow: root key 0, catalog keys 1 to 3.
        let draft = work.path().join("1.draft");
        fs::write(
            &draft,
            draft_trust(
                &repo,
                &TrustDraft {
                    publisher: Some("canopa"),
                    root_keys: &[keys.public(0)],
                    catalog_keys: &[keys.public(1), keys.public(2), keys.public(3)],
                    catalog_threshold: Some(2),
                    ..TrustDraft::default()
                },
            )?,
        )?;
        let signature = work.path().join("root.sig");
        fs::write(&signature, sign_trust(&draft, &keys.private(0))?)?;
        let v1 = commit_trust(&repo, &draft, &[signature], &[])?;
        assert!(show_trust(&repo)?.contains("Catalog keys (2 of 3 required):"));
        let pinned_v1 = read_trust_root(&v1)?;
        let specs = |indexes: &[usize]| -> Vec<PathBuf> {
            indexes.iter().map(|index| keys.private(*index)).collect()
        };
        for wrong in [&[1][..], &[1, 2, 3], &[0, 1]] {
            assert!(
                Publication::open(&repo, &specs(wrong)).is_err(),
                "{wrong:?}"
            );
        }
        let first = write_catalog(
            &repo,
            Catalog::empty("canopa"),
            &Publication::open(&repo, &specs(&[1, 2]))?,
        )?;
        read_catalog(&repo, &first, "canopa", &pinned_v1)?;

        // Revoke catalog key 1, then carry version 2 in a re-signed catalog.
        rotate_trust(
            &repo,
            &TrustDraft {
                catalog_keys: &[keys.public(2), keys.public(3)],
                ..TrustDraft::default()
            },
            &keys.private(0),
        )?;
        assert!(resign_catalog(&repo, "canopa", &specs(&[1, 2]), &first).is_err());
        let second = resign_catalog(&repo, "canopa", &specs(&[2, 3]), &first)?;
        let pinned_v2 = fs::read(trust_root_path(&repo, 2))?;
        let current = read_catalog(&repo, &second, "canopa", &pinned_v1)?;
        assert_eq!(current.trust.versions().len(), 2);

        // A holder of the revoked key signs under the old history.
        let stash = work.path().join("2.json");
        fs::rename(trust_root_path(&repo, 2), &stash)?;
        let mut base = read_root(&repo, &first, "canopa", &pinned_v1, Freshness::Pinned)?;
        base.sequence = 2;
        let forged_hash = write_catalog(&repo, base, &Publication::open(&repo, &specs(&[1, 2]))?)?;
        fs::rename(&stash, trust_root_path(&repo, 2))?;
        // The old history still verifies as an exact pin ...
        let forged = read_catalog(&repo, &forged_hash, "canopa", &pinned_v1)?;
        assert_eq!(forged.sequence, 3);
        // ... but cannot follow a catalog signed under the newer version,
        let error = current.check_successor(&forged, &second).unwrap_err();
        assert!(format!("{error:#}").contains("publisher trust root history"));
        // ... and a client pinned to version 2 rejects it outright.
        for hash in [&first, &forged_hash] {
            let error = read_catalog(&repo, hash, "canopa", &pinned_v2).unwrap_err();
            assert!(format!("{error:#}").contains("older than the pinned version 2"));
        }
        read_catalog(&repo, &second, "canopa", &pinned_v2)?;
        Ok(())
    }

    #[test]
    fn expired_trust_roots_stop_new_current_catalogs_until_renewed() -> Result<()> {
        let work = tempfile::tempdir()?;
        let key = work.path().join("key.der");
        fs::write(
            &key,
            Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap(),
        )?;
        // Repository setup syncs to disk and can be slow under load. Retry
        // with a longer lifetime if the trust root expired during setup.
        let mut attempt = None;
        for (index, seconds) in [5, 30, 120].into_iter().enumerate() {
            let repo = work.path().join(format!("repo-{index}"));
            let expires = jiff::Timestamp::now()
                .checked_add(jiff::Span::new().seconds(seconds))?
                .round(jiff::Unit::Second)?;
            let pin = read_trust_root(&init_trust(
                &repo,
                "canopa",
                &key,
                Some(&expires.to_string()),
            )?)?;
            let publication = Publication::open(&repo, &key)?;
            let first = write_catalog(&repo, Catalog::empty("canopa"), &publication)?;
            if read_current_catalog(&repo, "canopa", &pin).is_ok() {
                attempt = Some((repo, expires, pin, first));
                break;
            }
        }
        let (repo, expires, pin, first) = attempt.context("trust root setup kept expiring")?;
        while jiff::Timestamp::now() <= expires {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let error = read_current_catalog(&repo, "canopa", &pin).unwrap_err();
        assert!(format!("{error:#}").contains("expired"), "{error:#}");
        // An exact pin is still readable.
        read_catalog(&repo, &first, "canopa", &pin)?;
        let error = resign_catalog(&repo, "canopa", &key, &first).unwrap_err();
        assert!(format!("{error:#}").contains("expired"), "{error:#}");
        assert!(renew_trust(&repo, &key, 30)?);
        assert!(!renew_trust(&repo, &key, 30)?);
        resign_catalog(&repo, "canopa", &key, &first)?;
        let (catalog, _) = read_current_catalog(&repo, "canopa", &pin)?;
        assert_eq!(catalog.sequence, 2);
        Ok(())
    }

    #[test]
    fn companion_lineage_guards_primary_successors_and_matches_the_companion() -> Result<()> {
        let repo = tempfile::tempdir()?;
        prepare_objects(repo.path())?;
        let (signer, anchor) = test_publication(repo.path(), "seed")?;
        let anchor = anchor.as_slice();
        let sha256 = write_catalog(repo.path(), Catalog::empty("seed"), &signer)?;
        let lineage = CatalogLineage::new(&read_catalog(repo.path(), &sha256, "seed", anchor)?);
        assert_eq!(lineage.sequence, 1);
        let pin = CatalogPin {
            publisher: "seed".to_owned(),
            sha256,
            build_trust: None,
            lineage: Some(lineage),
        };
        pin.read(repo.path(), "seed", anchor)?;

        let mut previous = Catalog::empty("canopa");
        previous.profiles_required = true;
        previous.sequence = 1;
        previous.release_profiles.insert(
            "desktop".to_owned(),
            ReleaseProfile {
                architecture: "x86_64".to_owned(),
                facets: BTreeMap::new(),
                variants: BTreeMap::new(),
                facet_overrides: BTreeSet::new(),
                companions: vec![pin.clone()],
                roots: BTreeMap::new(),
                composition: BTreeMap::new(),
                elf_library_dirs: vec![],
                elf_symbol_versions: false,
                elf_require_declarations: false,
                rollout_percentage: None,
                advisories: Vec::new(),
            },
        );
        let previous_hash = digest(b"previous primary catalog");
        let mut next = previous.clone();
        next.sequence = 2;
        previous.check_successor(&next, &previous_hash)?;
        let changed_pin = |update: CatalogPin, expected: &str| {
            let mut successor = next.clone();
            successor
                .release_profiles
                .get_mut("desktop")
                .unwrap()
                .companions = vec![update];
            let error = previous
                .check_successor(&successor, &previous_hash)
                .unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        };
        let mut update = pin.clone();
        update.lineage = None;
        changed_pin(update, "release drops companion lineage");
        let mut update = pin.clone();
        update.sha256 = digest(b"companion catalog at the same sequence");
        changed_pin(update, "companion catalog rollback or fork");
        let mut update = pin.clone();
        update.lineage.as_mut().unwrap().trust_root_sha256 = digest(b"another trust root");
        changed_pin(update, "release changes companion trust anchor");
        let mut update = pin;
        update.lineage.as_mut().unwrap().sequence = 2;
        let error = update.read(repo.path(), "seed", anchor).unwrap_err();
        assert!(format!("{error:#}").contains("signed companion lineage differs"));
        changed_pin(update, "companion catalog rollback or fork");
        Ok(())
    }

    #[test]
    fn catalog_identity_is_the_digest_of_the_exact_signed_root() -> Result<()> {
        let repo = tempfile::tempdir()?;
        prepare_objects(repo.path())?;
        let (key, anchor) = test_publication(repo.path(), "canopa")?;
        let signer = SigningKey::open(&repo.path().join("signing-key.der"))?;
        let read = |hash: &str| read_catalog(repo.path(), hash, "canopa", &anchor);

        // The previous monolithic signed catalog format is not accepted.
        let legacy = r#"{"format":1,"publisher":"canopa","entries":{},"sequence":1}"#;
        let mut message = b"cps signed catalog v1\n".to_vec();
        message.extend_from_slice(legacy.as_bytes());
        let signed = format!(
            r#"{{"catalog":{legacy},"signature":"{}"}}"#,
            hex::encode(signer.sign(Purpose::Catalog, &message)?)
        );
        let hash = store(repo.path(), "catalogs", signed.as_bytes())?;
        assert!(read(&hash).is_err());

        let current = write_catalog(repo.path(), Catalog::empty("canopa"), &key)?;
        assert_eq!(read(&current)?.sequence, 1);
        let bytes = read_object(repo.path(), "catalogs", &current, CATALOG_ROOT_LIMIT)?;
        let payload = shards::open(&bytes)?.payload;
        // Any other spelling of the same root is a different, unsigned object.
        let mut envelope: serde_json::Value = serde_json::from_slice(&bytes)?;
        for respelled in [
            serde_json::to_vec_pretty(&envelope)?,
            [bytes.as_slice(), b"\n"].concat(),
        ] {
            let hash = store(repo.path(), "catalogs", &respelled)?;
            assert!(read(&hash).is_err());
        }
        let sig = envelope["signatures"][0]["sig"]
            .as_str()
            .unwrap()
            .to_uppercase();
        envelope["signatures"][0]["sig"] = sig.into();
        let hash = store(repo.path(), "catalogs", &serde_json::to_vec(&envelope)?)?;
        assert!(read(&hash).is_err());
        // Re-encoding the payload is not what was signed.
        let reencoded: serde_json::Value = serde_json::from_slice(&payload)?;
        let reencoded = serde_json::to_vec_pretty(&reencoded)?;
        envelope["signed"] =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, reencoded).into();
        let hash = store(repo.path(), "catalogs", &serde_json::to_vec(&envelope)?)?;
        let error = read(&hash).unwrap_err();
        assert!(
            format!("{error:#}").contains("catalog envelope")
                || format!("{error:#}").contains("invalid signature"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn signed_dependency_summaries_must_match_their_manifests() -> Result<()> {
        let manifest = |body: &str| {
            parse_manifest(&format!(
                "schema-version 1 kind=\"package-manifest\"\npackage \"apps/tool\" publisher=\"canopa\" version=\"1.0.0-0\" {{\n{body}}}\n"
            ))
        };
        let signed = manifest("    require \"library/one\"\n    provides \"cap/tool\"\n")?;
        let entry = CatalogEntry::new(digest(b"manifest"), &signed);
        entry.check_manifest(&signed)?;
        for changed in [
            "    require \"library/two\"\n    provides \"cap/tool\"\n",
            "    require \"library/one\"\n",
            "    provides \"cap/tool\"\n    require \"library/one\"\n    recommends \"x/y\"\n",
        ] {
            let error = entry.check_manifest(&manifest(changed)?).unwrap_err();
            assert!(format!("{error:#}").contains("dependency summary differs"));
        }
        // The summary round-trips through the shard encoding unchanged.
        let encoded = serde_json::to_vec(&entry)?;
        assert_eq!(serde_json::from_slice::<CatalogEntry>(&encoded)?, entry);
        Ok(())
    }

    #[test]
    fn successors_drop_withdrawals_only_through_the_signed_pruning_cutoff() -> Result<()> {
        let base = digest(b"previous catalog");
        let id = "cps://canopa/apps/old@1.0.0-0";
        let keep = "cps://canopa/apps/kept@1.0.0-0";
        let mut previous = Catalog::empty("canopa");
        for id in [id, keep] {
            previous.entries.insert(
                id.to_owned(),
                CatalogEntry::digest_only(digest(id.as_bytes())),
            );
        }
        previous.sequence = 1;
        previous.withdraw(id)?;
        previous.withdraw(keep)?;
        previous.sequence = 2;
        previous.release_profiles.insert(
            "desktop".to_owned(),
            ReleaseProfile {
                architecture: "x86_64".to_owned(),
                facets: BTreeMap::new(),
                companions: Vec::new(),
                roots: BTreeMap::new(),
                variants: BTreeMap::new(),
                facet_overrides: BTreeSet::new(),
                composition: BTreeMap::from([("apps/kept".to_owned(), digest(keep.as_bytes()))]),
                elf_library_dirs: Vec::new(),
                elf_symbol_versions: false,
                elf_require_declarations: false,
                rollout_percentage: None,
                advisories: Vec::new(),
            },
        );

        // Dropping a withdrawal without a cutoff, or rewriting it, is a fork.
        let mut next = previous.clone();
        next.sequence = 3;
        next.withdrawals.remove(id);
        let error = previous.check_successor(&next, &base).unwrap_err();
        assert!(format!("{error:#}").contains("drops or changes the withdrawal"));
        next.retired.insert(
            id.to_owned(),
            Retired {
                manifest: digest(id.as_bytes()),
                withdrawn: 2,
            },
        );
        let error = previous.check_successor(&next, &base).unwrap_err();
        assert!(format!("{error:#}").contains("drops or changes the withdrawal"));
        next.pruned_through = 2;
        previous.check_successor(&next, &base)?;
        next.retired.get_mut(id).unwrap().manifest = digest(b"another manifest");
        assert!(previous.check_successor(&next, &base).is_err());

        // Pruning keeps records that a signed composition names.
        let mut pruned = previous.clone();
        assert_eq!(pruned.prune_withdrawals(2)?, 1);
        assert!(pruned.withdrawals.contains_key(keep) && pruned.retired.contains_key(id));
        assert!(pruned.is_withdrawn(id) && !pruned.is_active(id));
        pruned.sequence = 3;
        previous.check_successor(&pruned, &base)?;
        let error = pruned.clone().prune_withdrawals(1).unwrap_err();
        assert!(format!("{error:#}").contains("must not move back"));
        assert!(pruned.clone().prune_withdrawals(4).is_err());

        // Tombstones are kept forever and the cutoff never moves back.
        let mut later = pruned.clone();
        later.sequence = 4;
        later.retired.clear();
        assert!(pruned.check_successor(&later, &base).is_err());
        let mut later = pruned.clone();
        later.sequence = 4;
        later.pruned_through = 1;
        assert!(pruned.check_successor(&later, &base).is_err());
        // An active package may be withdrawn and pruned between two catalogs.
        let mut skipped = Catalog::empty("canopa");
        skipped.sequence = 1;
        skipped.entries.insert(
            keep.to_owned(),
            CatalogEntry::digest_only(digest(keep.as_bytes())),
        );
        let mut later = skipped.clone();
        later.sequence = 5;
        later.entries.clear();
        later.pruned_through = 3;
        later.retired.insert(
            keep.to_owned(),
            Retired {
                manifest: digest(keep.as_bytes()),
                withdrawn: 2,
            },
        );
        skipped.check_successor(&later, &base)?;
        later.retired.get_mut(keep).unwrap().withdrawn = 1;
        assert!(skipped.check_successor(&later, &base).is_err());
        Ok(())
    }

    #[test]
    fn shards_partition_records_by_name_hash() -> Result<()> {
        let repo = tempfile::tempdir()?;
        prepare_objects(repo.path())?;
        let (key, anchor) = test_publication(repo.path(), "canopa")?;
        let mut catalog = Catalog::empty("canopa");
        for number in 0..64 {
            let id = format!("cps://canopa/bulk/pkg-{number}@1.0.0-0");
            let entry = CatalogEntry::digest_only(digest(id.as_bytes()));
            catalog.entries.insert(id, entry);
        }
        let hash = write_catalog(repo.path(), catalog, &key)?;
        let read = read_catalog(repo.path(), &hash, "canopa", &anchor)?;
        assert_eq!(read.entries.len(), 64);
        for (prefix, shard) in &read.layout.loaded {
            assert!(
                shard
                    .entries
                    .keys()
                    .all(|id| shard_of_id(id).unwrap() == *prefix)
            );
        }
        // A shard holding a record of another prefix is rejected.
        let (prefix, shard) = read.layout.loaded.iter().next().unwrap();
        let mut forged = (**shard).clone();
        let foreign = (0..)
            .map(|number| format!("cps://canopa/other/pkg-{number}@1.0.0-0"))
            .find(|id| shard_of_id(id).unwrap() != *prefix)
            .unwrap();
        forged
            .entries
            .insert(foreign, CatalogEntry::digest_only(digest(b"foreign")));
        let bytes = forged.encode()?;
        assert!(shards::decode_shard(&bytes, &digest(&bytes)).is_err());

        // The self-contained archive round-trips and checks every shard.
        let archive = catalog_bytes(repo.path(), &hash)?;
        assert_eq!(archived_catalog_hash(&archive)?, hash);
        let decoded = decode_archived_catalog(&archive, &hash, "canopa", &anchor)?;
        assert_eq!(decoded.entries, read.entries);
        assert!(decode_archived_catalog(&archive, &digest(b"x"), "canopa", &anchor).is_err());
        Ok(())
    }

    #[test]
    fn catalog_successors_preserve_signed_renames() -> Result<()> {
        let mut previous = Catalog::empty("canopa");
        let old = "cps://canopa/apps/old@1.0.0-0";
        let new = "cps://canopa/apps/new@1.0.0-0";
        for id in [old, new] {
            previous.entries.insert(
                id.to_owned(),
                CatalogEntry::digest_only(digest(id.as_bytes())),
            );
        }
        previous.rename("cps://canopa/apps/old@2.0.0-0", "apps/new".into())?;
        previous.sequence = 1;
        let base = digest(b"previous catalog");
        let mut next = previous.clone();
        next.sequence = 2;
        next.withdraw(new)?;
        assert_eq!(next.rename_targets()?["apps/old"], "apps/new");
        previous.check_successor(&next, &base)?;
        next.renames.clear();
        let error = previous.check_successor(&next, &base).unwrap_err();
        assert!(format!("{error:#}").contains("drops or changes the signed rename"));
        next.renames.insert("apps/old".into(), "apps/other".into());
        assert!(previous.check_successor(&next, &base).is_err());
        Ok(())
    }

    #[test]
    fn catalog_successors_preserve_signed_publication_records() -> Result<()> {
        let base = digest(b"base catalog");
        let active = "cps://canopa/desktop@2.0.0-0".to_owned();
        let withdrawn = "cps://canopa/desktop@1.0.0-0".to_owned();
        let manifest = CatalogEntry::digest_only(digest(b"active manifest"));
        let attestation = digest(b"build evidence");
        let previous = Catalog {
            publisher: "canopa".to_owned(),
            entries: BTreeMap::from([(active.clone(), manifest.clone())]),
            file_index: false,
            attestations: BTreeMap::from([(active.clone(), attestation.clone())]),
            attestations_required: true,
            toolchains_verified: true,
            build_inputs: BTreeMap::from([(
                active.clone(),
                serde_json::from_value(serde_json::json!({
                    "recipe": digest(b"reviewed recipe"),
                    "templates": [digest(b"reviewed manifest")],
                }))?,
            )]),
            source_archives_required: false,
            withdrawals: BTreeMap::from([(
                withdrawn.clone(),
                Withdrawal {
                    manifest: CatalogEntry::digest_only(digest(b"withdrawn manifest")),
                    attestation: Some(digest(b"withdrawn build evidence")),
                    sequence: 1,
                },
            )]),
            retired: BTreeMap::new(),
            pruned_through: 0,
            sequence: 1,
            profiles_required: true,
            obsoletions: BTreeMap::from([("desktop/retired".to_owned(), "2.0.0-0".parse()?)]),
            renames: BTreeMap::new(),
            trust: TrustChain::default(),
            trust_keys: BTreeSet::new(),
            trust_genesis: String::new(),
            release_profiles: BTreeMap::from([(
                "desktop".to_owned(),
                ReleaseProfile {
                    architecture: "x86_64".to_owned(),
                    facets: BTreeMap::new(),
                    variants: BTreeMap::new(),
                    facet_overrides: BTreeSet::new(),
                    companions: Vec::new(),
                    roots: BTreeMap::new(),
                    composition: BTreeMap::new(),
                    elf_library_dirs: vec!["/usr/lib".to_owned()],
                    elf_symbol_versions: true,
                    elf_require_declarations: false,
                    rollout_percentage: None,
                    advisories: Vec::new(),
                },
            )]),
            layout: Layout::default(),
        };
        let mut next = previous.clone();
        next.sequence = 3;
        previous.check_successor(&next, &base)?;

        let inputs = next.build_inputs.remove(&active).unwrap();
        let error = previous.check_successor(&next, &base).unwrap_err();
        assert!(format!("{error:#}").contains("drops or changes the rebuild inputs"));
        next.build_inputs.insert(active.clone(), inputs);
        let mut with_sources = previous.clone();
        with_sources
            .build_inputs
            .get_mut(&active)
            .unwrap()
            .sources_archived = true;
        let error = with_sources.check_successor(&next, &base).unwrap_err();
        assert!(format!("{error:#}").contains("drops or changes the rebuild inputs"));
        next.build_inputs.get_mut(&active).unwrap().sources_archived = true;
        previous.check_successor(&next, &base)?;
        with_sources.source_archives_required = true;
        let error = with_sources.check_source_archives().unwrap_err();
        assert!(format!("{error:#}").contains(&withdrawn));
        let error = with_sources.check_successor(&next, &base).unwrap_err();
        assert!(
            format!("{error:#}").contains("disables the publisher's source archive requirement")
        );
        let added = "cps://canopa/new@1.0.0-0";
        next.entries.insert(added.to_owned(), manifest.clone());
        let error = previous.check_successor(&next, &base).unwrap_err();
        assert!(format!("{error:#}").contains("omits rebuild inputs for new package"));
        next.entries.remove(added);

        next.attestations_required = false;
        assert!(previous.check_successor(&next, &base).is_err());
        next.attestations_required = true;
        next.toolchains_verified = false;
        let error = previous.check_successor(&next, &base).unwrap_err();
        assert!(format!("{error:#}").contains("disables the publisher's toolchain verification"));
        let mut unchecked = previous.clone();
        unchecked.toolchains_verified = false;
        next.toolchains_verified = true;
        let error = unchecked.check_successor(&next, &base).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("claims toolchain verification after packages were published")
        );

        next.profiles_required = false;
        assert!(previous.check_successor(&next, &base).is_err());
        next.profiles_required = true;
        next.release_profiles
            .get_mut("desktop")
            .unwrap()
            .elf_symbol_versions = false;
        assert!(
            previous
                .check_successor(&next, &base)
                .unwrap_err()
                .to_string()
                .contains("ELF symbol version checking")
        );
        next.release_profiles
            .get_mut("desktop")
            .unwrap()
            .elf_symbol_versions = true;
        next.obsoletions.clear();
        assert!(previous.check_successor(&next, &base).is_err());
        next.obsoletions
            .insert("desktop/retired".to_owned(), "1.0.0-0".parse()?);
        assert!(previous.check_successor(&next, &base).is_err());
        next.obsoletions
            .insert("desktop/retired".to_owned(), "3.0.0-0".parse()?);
        previous.check_successor(&next, &base)?;

        next.attestations.remove(&active);
        assert!(previous.check_successor(&next, &base).is_err());
        next.entries.remove(&active);
        assert!(previous.check_successor(&next, &base).is_err());
        next.withdrawals.insert(
            active,
            Withdrawal {
                manifest,
                attestation: Some(attestation),
                sequence: 2,
            },
        );
        previous.check_successor(&next, &base)?;

        next.withdrawals.remove(&withdrawn);
        assert!(previous.check_successor(&next, &base).is_err());
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
    fn rejects_old_json_manifests() -> Result<()> {
        let repo = tempfile::tempdir()?;
        fs::create_dir(repo.path().join("manifests"))?;
        let bytes = br#"{"format":1,"publisher":"canopa","name":"test/legacy","version":"1.0.0-0","dependency":[{"kind":"require","package":"library/one","version":null},{"kind":"incorporate","package":"library/two","version":"=1.0.0-0"}],"action":[]}"#;
        let hash = store(repo.path(), "manifests", bytes)?;
        assert!(load_manifest(repo.path(), hash.clone()).is_err());
        let old_record = serde_json::json!({
            "sha256": hash,
            "manifest": serde_json::from_slice::<Manifest>(bytes)?,
        });
        assert!(serde_json::from_value::<Published>(old_record).is_err());
        Ok(())
    }
}
