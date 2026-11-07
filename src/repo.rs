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
    attestation::{ATTESTATION_LIMIT, BuildTrust, MAX_BUILDERS, VerifiedBuild, verify_bundle},
    image::root::Root,
    kdl_format::{manifest_text, parse_manifest, publish_policy, publish_release},
    model::{
        FORMAT, FileDigest, Manifest, Node, PackageSpec, Version, copy_payload, digest,
        digest_reader, validate_digest, validate_name, validate_publisher, validate_summary,
    },
    systemd::{self, Action as SystemdAction},
};

pub(crate) const CATALOG_LIMIT: u64 = 64 * 1024 * 1024;
pub(crate) const MANIFEST_LIMIT: u64 = 32 * 1024 * 1024;
const FILE_INDEX_OBJECTS: &str = "file-indexes";
const FILE_INDEX_LIMIT: u64 = 64 * 1024 * 1024;
const TMPFILES_LIMIT: u64 = 1024 * 1024;
const PUBLISH_POLICY_LIMIT: u64 = 1024 * 1024;
const PUBLISH_RELEASE_LIMIT: u64 = 1024 * 1024;
const PUBLISH_POLICY: &str = "publish-policy.kdl";

mod keys;
pub(crate) mod profiles;
mod rebuild;
pub mod release;
mod source_archive;
pub use rebuild::{export_rebuild_inputs, verify_rebuild};
pub use source_archive::{archive_build_sources, fetch_build_sources};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Catalog {
    format: u32,
    publisher: String,
    entries: BTreeMap<String, CatalogEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_index: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    attestations: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    attestations_required: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    toolchains_verified: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    build_inputs: BTreeMap<String, rebuild::Digests>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    source_archives_required: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    withdrawals: BTreeMap<String, Withdrawal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sequence: Option<NonZeroU64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    profiles_required: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    obsoletions: BTreeMap<String, Version>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    renames: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    key_rotations: Vec<keys::Rotation>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    release_profiles: BTreeMap<String, ReleaseProfile>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseProfile {
    pub architecture: String,
    pub facets: BTreeMap<String, bool>,
    pub companions: Vec<CatalogPin>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roots: BTreeMap<String, Version>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub composition: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub elf_library_dirs: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub elf_symbol_versions: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_percentage: Option<u8>,
}

impl ReleaseProfile {
    pub(crate) fn check_image<'a>(
        &self,
        name: &str,
        arch: &str,
        facets: &BTreeMap<String, bool>,
        companions: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        ensure!(
            self.architecture == arch && &self.facets == facets,
            "image architecture or facets differ from signed release profile {name}"
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
            self.composition.is_empty() || !self.roots.is_empty(),
            "signed profile {name} has a composition without roots"
        );
        if let Some(percentage) = self.rollout_percentage {
            ensure!(
                percentage <= 100 && !self.composition.is_empty(),
                "signed profile {name} requires a composition and a rollout percentage from 0 to 100"
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
                self.composition.is_empty()
                    || self.composition.get(root).map(String::as_str)
                        == Some(entry.manifest_digest()),
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
    anchor_sha256: String,
    sequence: u64,
}

impl CatalogLineage {
    fn new(key: &[u8], catalog: &Catalog) -> Self {
        Self {
            anchor_sha256: digest(key),
            sequence: catalog.sequence.map_or(0, NonZeroU64::get),
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
        self.check_lineage(&catalog, key)?;
        Ok(catalog)
    }

    fn check_lineage(&self, catalog: &Catalog, key: &[u8]) -> Result<()> {
        if let Some(lineage) = &self.lineage {
            ensure!(
                lineage == &CatalogLineage::new(key, catalog),
                "signed companion lineage differs from catalog {} for {}",
                self.sha256,
                self.publisher
            );
        }
        Ok(())
    }

    pub(crate) fn check_archive(&self, catalog: &Catalog, hash: &str, key: &[u8]) -> Result<()> {
        ensure!(
            self.publisher == catalog.publisher && self.sha256 == hash,
            "archived catalog for {} differs from the signed profile pin",
            self.publisher
        );
        self.check_lineage(catalog, key)
    }

    pub(crate) fn snapshot(&self, repo: &Path, publisher: &str, key: &[u8]) -> Result<Snapshot> {
        let catalog = self.read(repo, publisher, key)?;
        catalog_snapshot(repo, catalog, self.sha256.clone(), key)
    }
}

impl Catalog {
    pub(crate) fn checkpoint(&self, catalog: String) -> CatalogCheckpoint {
        CatalogCheckpoint {
            catalog,
            sequence: self.sequence,
            rotations: self.key_rotations.clone(),
        }
    }

    fn empty(publisher: &str) -> Self {
        Self {
            format: FORMAT,
            publisher: publisher.to_owned(),
            entries: BTreeMap::new(),
            file_index: None,
            attestations: BTreeMap::new(),
            attestations_required: false,
            toolchains_verified: false,
            build_inputs: BTreeMap::new(),
            source_archives_required: false,
            withdrawals: BTreeMap::new(),
            sequence: None,
            profiles_required: false,
            obsoletions: BTreeMap::new(),
            renames: BTreeMap::new(),
            key_rotations: Vec::new(),
            release_profiles: BTreeMap::new(),
        }
    }

    fn package_record(&self, id: &str) -> Option<(&CatalogEntry, Option<&str>)> {
        if let Some(entry) = self.entries.get(id) {
            Some((entry, self.attestations.get(id).map(String::as_str)))
        } else {
            let withdrawn = self.withdrawals.get(id)?;
            Some((&withdrawn.manifest, withdrawn.attestation.as_deref()))
        }
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
        let (entry, attestation) = self
            .package_record(&package.manifest.id())
            .context("package is absent from the signed catalog")?;
        ensure!(
            entry.manifest_digest() == package.sha256,
            "installed manifest digest differs from the signed catalog"
        );
        entry.check_manifest(&package.manifest)?;
        Ok(attestation)
    }

    pub(crate) fn publisher_keys(&self, anchor: &[u8]) -> Result<Vec<[u8; 32]>> {
        let mut keys = vec![
            anchor
                .try_into()
                .context("invalid Ed25519 public key length")?,
        ];
        keys.extend(
            self.key_rotations
                .iter()
                .map(|rotation| *rotation.next_key()),
        );
        Ok(keys)
    }

    fn take_build_evidence(&mut self, hash: &str, key: &[u8]) -> Result<BuildEvidence> {
        let publisher_keys = self.publisher_keys(key)?;
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
                rotations: std::mem::take(&mut self.key_rotations),
            },
        })
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
                cutoff >= *previous,
                "obsoletion must advance the cutoff for {}",
                spec.name
            );
            if cutoff == *previous {
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
            .take_while(|id| id.starts_with(&prefix));
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
                cutoff > version,
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
                self.obsoletions.get(spec.name).copied() == spec.version,
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
        for id in self.withdrawals.keys() {
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
                .peekable();
            ensure!(
                withdrawn.peek().is_some(),
                "no published versions to rename for {name}"
            );
            for id in withdrawn {
                ensure!(
                    PackageSpec::parse(id)?
                        .version
                        .is_some_and(|version| version < *cutoff),
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

    pub(crate) fn check_successor(&self, next: &Self, previous_hash: &str) -> Result<()> {
        check_catalog_sequence(self.sequence, next.sequence)?;
        keys::check_successor(
            &self.key_rotations,
            self.sequence,
            previous_hash,
            &next.key_rotations,
        )?;
        ensure!(
            self.file_index.is_none() || next.file_index.is_some(),
            "release drops the publisher's file index"
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
            !next.toolchains_verified
                || self.toolchains_verified
                || (self.entries.is_empty() && self.withdrawals.is_empty()),
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
        for (id, record) in &self.withdrawals {
            ensure!(
                next.withdrawals.get(id) == Some(record),
                "release drops or changes the withdrawal for {id}"
            );
        }
        for (name, cutoff) in &self.obsoletions {
            ensure!(
                next.obsoletions
                    .get(name)
                    .is_some_and(|next| next >= cutoff),
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
                next.build_inputs
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
                        previous.anchor_sha256 == lineage.anchor_sha256,
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

    fn validate_profile_build_trust(&self, signer: &[u8]) -> Result<()> {
        for pin in self
            .release_profiles
            .values()
            .flat_map(|profile| &profile.companions)
        {
            if let Some(trust) = &pin.build_trust {
                trust.validate()?;
                trust.check_publisher_key(signer)?;
                for rotation in &self.key_rotations {
                    trust.check_publisher_key(rotation.previous_key())?;
                }
            }
        }
        Ok(())
    }

    fn next_sequence(&self) -> Result<NonZeroU64> {
        self.sequence
            .map_or(0, NonZeroU64::get)
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .context("publisher catalog sequence exhausted")
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum CatalogEntry {
    Portable(String),
    Metadata {
        manifest: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        architecture: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
}

impl CatalogEntry {
    pub(crate) fn manifest_digest(&self) -> &str {
        match self {
            Self::Portable(manifest) | Self::Metadata { manifest, .. } => manifest,
        }
    }

    fn architecture(&self) -> Option<&str> {
        match self {
            Self::Portable(_) => None,
            Self::Metadata { architecture, .. } => architecture.as_deref(),
        }
    }

    pub(crate) fn summary(&self) -> Option<&str> {
        match self {
            Self::Portable(_) => None,
            Self::Metadata { summary, .. } => summary.as_deref(),
        }
    }

    pub(crate) fn supports_architecture(&self, architecture: &str) -> bool {
        self.architecture()
            .is_none_or(|target| target == architecture)
    }

    fn validate(&self) -> Result<()> {
        validate_digest(self.manifest_digest())?;
        ensure!(
            matches!(self, Self::Portable(_))
                || self.architecture().is_some()
                || self.summary().is_some(),
            "catalog package metadata is empty"
        );
        if let Some(architecture) = self.architecture() {
            validate_publisher(architecture).context("invalid catalog package architecture")?;
        }
        if let Some(summary) = self.summary() {
            validate_summary(summary)?;
        }
        Ok(())
    }

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
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Withdrawal {
    manifest: CatalogEntry,
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
    pub(crate) builds: BuildEvidence,
    build_inputs: BTreeMap<String, rebuild::Digests>,
    file_index: Option<String>,
    withdrawals: BTreeMap<String, Withdrawal>,
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
    sequence: Option<NonZeroU64>,
    rotations: Vec<keys::Rotation>,
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
            self.sequence == catalog.sequence && self.rotations == catalog.key_rotations,
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
        keys::check_successor(
            &self.rotations,
            self.sequence,
            &self.catalog,
            &next.rotations,
        )
    }
}

fn check_catalog_sequence(previous: Option<NonZeroU64>, next: Option<NonZeroU64>) -> Result<()> {
    if let Some(previous) = previous {
        ensure!(
            next.is_some_and(|sequence| sequence > previous),
            "publisher catalog rollback or fork after sequence {previous}"
        );
    }
    Ok(())
}

/// Local record of an authenticated catalog's admission of a manifest.
/// The envelope and publisher key transitions are independently verifiable.
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
        let signer = keys::signer(
            &self.publisher,
            self.history.sequence,
            &self.history.rotations,
            keys::Trust::Anchor(anchor),
        )?;
        for rotation in &self.history.rotations {
            trust.check_publisher_key(rotation.previous_key())?;
        }
        trust.check_publisher_key(&signer)?;
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
    let document = package
        .document
        .as_ref()
        .context("build trust requires a published KDL manifest")?;
    trust
        .verify(
            bytes,
            &package.manifest,
            FileDigest {
                sha256: package.sha256.clone(),
                size: document.len() as u64,
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
    for key in catalog.publisher_keys(key)? {
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
    read_catalog_with_key(repo, hash, publisher, keys::Trust::Anchor(key))
}

fn read_catalog_with_key(
    repo: &Path,
    hash: &str,
    publisher: &str,
    trust: keys::Trust<'_>,
) -> Result<Catalog> {
    let bytes = catalog_bytes(repo, hash)?;
    decode_catalog(&bytes, hash, publisher, trust)
}

pub(crate) fn catalog_bytes(repo: &Path, hash: &str) -> Result<Vec<u8>> {
    read_object(repo, "catalogs", hash, CATALOG_LIMIT)
}

pub(crate) fn export_locked_repository(
    publisher: &crate::image::Publisher,
    hash: &str,
    packages: &[&Published],
    blobs: &BTreeMap<&str, &Node>,
    destination: &Path,
) -> Result<()> {
    let bytes = catalog_bytes(&publisher.repo, hash)?;
    let catalog = decode_catalog(
        &bytes,
        hash,
        &publisher.name,
        keys::Trust::Anchor(&publisher.key),
    )?;
    fs::create_dir(destination)?;
    prepare_objects(destination)?;
    for package in packages {
        catalog.check_installed(package)?;
    }
    // The planner reads the whole pinned index, but only the locked selection
    // needs attestations and payloads to provision this image.
    let manifests = catalog
        .entries
        .values()
        .map(CatalogEntry::manifest_digest)
        .collect::<BTreeSet<_>>();
    let attestations = packages
        .iter()
        .filter_map(|package| catalog.attestations.get(&package.manifest.id()))
        .collect::<BTreeSet<_>>();
    for (kind, hashes, limit) in [
        ("manifests", manifests, MANIFEST_LIMIT),
        (
            FILE_INDEX_OBJECTS,
            catalog.file_index.iter().map(String::as_str).collect(),
            FILE_INDEX_LIMIT,
        ),
        (
            "attestations",
            attestations.into_iter().map(String::as_str).collect(),
            ATTESTATION_LIMIT,
        ),
    ] {
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
    write_atomic(&destination.join("catalogs").join(hash), &bytes, true)?;
    Ok(())
}

fn decode_catalog(
    bytes: &[u8],
    hash: &str,
    publisher: &str,
    trust: keys::Trust<'_>,
) -> Result<Catalog> {
    let signed: SignedCatalog = serde_json::from_slice(bytes)?;
    let catalog = signed.catalog;
    ensure!(catalog.format == FORMAT, "unsupported catalog format");
    if let Some(hash) = &catalog.file_index {
        validate_digest(hash).context("invalid file index digest")?;
    }
    ensure!(
        catalog.publisher == publisher,
        "unexpected publisher {}",
        catalog.publisher
    );
    let signer = keys::signer(publisher, catalog.sequence, &catalog.key_rotations, trust)?;
    UnparsedPublicKey::new(&ED25519, signer)
        .verify(
            &serde_json::to_vec(&catalog)?,
            &hex::decode(signed.signature)?,
        )
        .map_err(|_| anyhow::anyhow!("invalid signature on catalog {hash}"))?;
    ensure!(
        catalog.release_profiles.is_empty() || catalog.profiles_required,
        "signed composition profiles require the publishing gate"
    );
    catalog.validate_profile_build_trust(&signer)?;
    for (name, profile) in &catalog.release_profiles {
        validate_name(name).context("invalid signed profile name")?;
        validate_publisher(&profile.architecture).context("invalid signed profile architecture")?;
        ensure!(
            !profile.elf_symbol_versions || !profile.elf_library_dirs.is_empty(),
            "ELF symbol version checking requires library directories"
        );
        if !profile.elf_library_dirs.is_empty() {
            publish_policy::validate_elf_dirs(&profile.architecture, &profile.elf_library_dirs)?;
        }
        for facet in profile.facets.keys() {
            validate_publisher(facet).context("invalid signed profile facet")?;
        }
        profile.validate_composition(name, &catalog)?;
        let mut publishers = BTreeSet::from([publisher]);
        for pin in &profile.companions {
            validate_publisher(&pin.publisher)?;
            validate_digest(&pin.sha256)?;
            if let Some(lineage) = &pin.lineage {
                validate_digest(&lineage.anchor_sha256)?;
            }
            ensure!(
                publishers.insert(pin.publisher.as_str()),
                "duplicate publisher {} in signed profile {name}",
                pin.publisher
            );
        }
    }
    for (id, entry) in &catalog.entries {
        let spec = PackageSpec::parse(id)?;
        let version = spec
            .version
            .with_context(|| format!("catalog entry {id} has no version"))?;
        ensure!(
            spec.publisher == Some(publisher),
            "invalid catalog entry {id}"
        );
        ensure!(
            catalog
                .obsoletions
                .get(spec.name)
                .is_none_or(|cutoff| version > *cutoff),
            "catalog advertises obsolete package {id}"
        );
        entry.validate()?;
    }
    for (name, cutoff) in &catalog.obsoletions {
        validate_name(name)?;
        ensure!(
            !catalog
                .withdrawals
                .contains_key(&format!("cps://{publisher}/{name}@{cutoff}")),
            "obsoletion for {name} reuses a published version"
        );
    }
    catalog.rename_targets()?;
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
    ensure!(
        !catalog.toolchains_verified || catalog.attestations_required,
        "toolchain verification requires signed build evidence"
    );
    for (id, withdrawal) in &catalog.withdrawals {
        ensure!(
            exact_publisher(id)? == publisher && !catalog.entries.contains_key(id),
            "invalid withdrawal for {id}"
        );
        withdrawal.manifest.validate()?;
        ensure!(
            !catalog.toolchains_verified || withdrawal.attestation.is_some(),
            "toolchain-verified catalog withdrew a package without build evidence"
        );
        if let Some(digest) = &withdrawal.attestation {
            validate_digest(digest)?;
            ensure!(
                evidence.insert(digest),
                "duplicate package attestation {digest}"
            );
        }
    }
    for (id, inputs) in &catalog.build_inputs {
        ensure!(
            catalog.attestations_required
                && catalog
                    .package_record(id)
                    .is_some_and(|(_, attestation)| attestation.is_some()),
            "rebuild inputs require an admitted build attestation for {id}"
        );
        for (hash, _) in inputs.objects() {
            validate_digest(hash).context("invalid rebuild input digest")?;
        }
    }
    catalog.check_source_archives()?;
    Ok(catalog)
}

pub fn read_attestation(repo: &Path, publisher: &str, key: &Path, id: &str) -> Result<Vec<u8>> {
    let catalog = read_catalog(repo, &read_current(repo)?, publisher, &read_key(key)?)?;
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
    let hash = match pinned {
        Some(hash) => hash.to_owned(),
        None => read_current(repo)?,
    };
    let catalog = read_catalog(repo, &hash, publisher, key)?;
    catalog_snapshot(repo, catalog, hash, key)
}

fn catalog_snapshot(
    repo: &Path,
    mut catalog: Catalog,
    hash: String,
    key: &[u8],
) -> Result<Snapshot> {
    let rename_targets = catalog.rename_targets()?;
    let builds = catalog.take_build_evidence(&hash, key)?;
    let mut packages = Vec::with_capacity(catalog.entries.len());
    for (id, entry) in catalog.entries {
        packages.push(read_package(repo, &catalog.publisher, &id, &entry)?);
    }
    Ok(Snapshot {
        sha256: hash,
        packages,
        builds,
        build_inputs: catalog.build_inputs,
        file_index: catalog.file_index,
        withdrawals: catalog.withdrawals,
        obsoletions: catalog.obsoletions,
        rename_targets,
    })
}

pub(crate) struct CatalogIndex {
    pub entries: BTreeMap<String, CatalogEntry>,
    pub obsoletions: BTreeMap<String, Version>,
    pub checkpoint: CatalogCheckpoint,
    pub file_index: Option<String>,
}

pub(crate) fn catalog_index(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    pin: Option<&CatalogPin>,
) -> Result<CatalogIndex> {
    let (catalog, hash) = read_selected_catalog(repo, publisher, key, pin)?;
    Ok(CatalogIndex {
        entries: catalog.entries,
        obsoletions: catalog.obsoletions,
        file_index: catalog.file_index,
        checkpoint: CatalogCheckpoint {
            catalog: hash,
            sequence: catalog.sequence,
            rotations: catalog.key_rotations,
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
    let hash = read_current(repo)?;
    Ok((read_catalog(repo, &hash, publisher, key)?, hash))
}

pub(crate) fn release_profile(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    pin: Option<&str>,
    name: &str,
) -> Result<(CatalogCheckpoint, ReleaseProfile)> {
    let hash = pin.map_or_else(|| read_current(repo), |hash| Ok(hash.to_owned()))?;
    let mut catalog = read_catalog(repo, &hash, publisher, key)?;
    let profile = take_profile(&mut catalog, &hash, name)?;
    Ok((
        CatalogCheckpoint {
            catalog: hash,
            sequence: catalog.sequence,
            rotations: catalog.key_rotations,
        },
        profile,
    ))
}

pub(crate) fn decode_archived_catalog(
    bytes: &[u8],
    hash: &str,
    publisher: &str,
    key: &[u8],
) -> Result<Catalog> {
    ensure!(digest(bytes) == hash, "archived catalog digest mismatch");
    decode_catalog(bytes, hash, publisher, keys::Trust::Anchor(key))
}

pub(crate) fn check_catalog_successor(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    previous_hash: &str,
    archived: Option<&[u8]>,
    next_hash: &str,
) -> Result<()> {
    let previous = if let Some(bytes) = archived {
        decode_archived_catalog(bytes, previous_hash, publisher, key)?
    } else {
        validate_digest(previous_hash)?;
        if !path_present(&repo.join("catalogs").join(previous_hash))? {
            // Images created by older clients may only have a compact checkpoint.
            return Ok(());
        }
        read_catalog(repo, previous_hash, publisher, key)?
    };
    let next = read_catalog(repo, next_hash, publisher, key)?;
    previous.check_successor(&next, previous_hash)
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
    let index = catalog_index(repo, publisher, &read_key(key)?, None)?;
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
    for dir in [
        "catalogs",
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

pub fn publish(repo: &Path, proto: &Path, recipe: &Path, key: &Path) -> Result<String> {
    let key = read_signing_key(key)?;
    let manifest_bytes = read_limited(File::open(recipe)?, MANIFEST_LIMIT)?;
    let manifest = parse_manifest(std::str::from_utf8(&manifest_bytes)?)
        .with_context(|| format!("parsing {}", recipe.display()))?;
    manifest.validate(false)?;
    publish_manifest(repo, Root::open(proto)?, manifest, &key, None, None)
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
    let current = read_current(repo)?;
    let mut catalog = read_catalog_with_key(
        repo,
        &current,
        publisher,
        keys::Trust::Current(key.public_key().as_ref()),
    )?;
    if catalog.withdraw(id)? {
        let policy = read_publish_policy(repo, &catalog, false)?;
        check_profiles(
            repo,
            &mut catalog,
            Some(&current),
            policy.as_ref(),
            key.public_key().as_ref(),
        )?;
        write_catalog(repo, catalog, &key)?;
    }
    Ok(())
}

pub fn rotate_publisher_key(
    repo: &Path,
    publisher: &str,
    base: &str,
    old_key: &Path,
    new_key: &Path,
) -> Result<String> {
    validate_publisher(publisher)?;
    validate_digest(base)?;
    let old_key = read_signing_key(old_key)?;
    let new_key = read_signing_key(new_key)?;
    Root::open(repo)?;
    let _lock = lock_repo(repo)?;
    let current = publication_base(repo)?.context("repository has no catalog to rotate")?;
    ensure!(
        current == base,
        "rotation base is stale: expected {base}, current is {current}"
    );
    let mut catalog = publication_catalog(repo, publisher, &old_key, Some(&current))?;
    let rotation = keys::Rotation::new(
        publisher,
        &current,
        catalog.next_sequence()?,
        &old_key,
        &new_key,
    )?;
    catalog.key_rotations.push(rotation);
    write_catalog(repo, catalog, &new_key)
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
        .map(|(id, withdrawal)| (id, withdrawal.manifest.manifest_digest().to_owned()))
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
    key: &Path,
    builder_key: &Path,
    reviewed_recipe: &str,
    witnesses: &[BuildWitness],
    source_cache: Option<&Path>,
) -> Result<String> {
    let key = read_signing_key(key)?;
    let (proto, manifest, verified) = verify_publication(
        bundle,
        builder_key,
        reviewed_recipe,
        witnesses,
        key.public_key().as_ref(),
    )?;
    let source_cache = source_cache.map(Root::open).transpose()?;
    publish_manifest(
        repo,
        proto,
        manifest,
        &key,
        Some(verified),
        source_cache.as_ref(),
    )
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
    publish_release_with_sources(repo, key, document, None)
}

pub fn publish_release_with_sources(
    repo: &Path,
    key: &Path,
    document: &Path,
    source_cache: Option<&Path>,
) -> Result<String> {
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
                    source_cache.as_ref(),
                )
                .with_context(|| format!("staging {id}"))?;
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
            key.public_key().as_ref(),
        )?;
        if previous.is_none_or(|profiles| profiles != catalog.release_profiles) {
            return write_catalog(repo, catalog, &key);
        }
    }
    current.context("empty release has no published catalog")
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
        Some(hash) => read_catalog_with_key(
            repo,
            hash,
            publisher,
            keys::Trust::Current(key.public_key().as_ref()),
        ),
        None => Ok(Catalog::empty(publisher)),
    }
}

fn publish_manifest(
    repo: &Path,
    proto: Root,
    manifest: Manifest,
    key: &Ed25519KeyPair,
    evidence: Option<VerifiedBuild>,
    source_cache: Option<&Root>,
) -> Result<String> {
    let _lock = lock_repo(repo)?;
    let current = publication_base(repo)?;
    let mut catalog = publication_catalog(repo, &manifest.publisher, key, current.as_deref())?;
    let id = manifest.id();
    ensure!(
        !catalog.renames.contains_key(&manifest.name),
        "cannot publish {id} under a renamed package name"
    );
    let policy = read_publish_policy(repo, &catalog, true)?;
    prepare_objects(repo)?;
    if stage_manifest(
        repo,
        &mut catalog,
        proto,
        manifest,
        evidence,
        policy.as_ref(),
        source_cache,
    )? {
        check_profiles(
            repo,
            &mut catalog,
            current.as_deref(),
            policy.as_ref(),
            key.public_key().as_ref(),
        )?;
        write_catalog(repo, catalog, key)?;
    }
    Ok(id)
}

fn stage_manifest(
    repo: &Path,
    catalog: &mut Catalog,
    proto: Root,
    manifest: Manifest,
    evidence: Option<VerifiedBuild>,
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
        !catalog.withdrawals.contains_key(&id),
        "{id} was withdrawn and cannot be republished"
    );
    if let Some(cutoff) = catalog.obsoletions.get(&manifest.name) {
        ensure!(
            manifest.version > *cutoff,
            "{id} is obsolete through {cutoff}"
        );
    }
    if let Some(policy) = policy {
        policy.verify(&id, evidence.as_ref())?;
        catalog.attestations_required = true;
        catalog.toolchains_verified |= policy.verify_toolchains;
    }
    if let Some(evidence) = &evidence {
        for rotation in &catalog.key_rotations {
            let retired = digest(rotation.previous_key());
            ensure!(
                evidence.builders().all(|builder| builder != retired),
                "a retired publisher key cannot sign builds"
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
                .as_ref()
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
    let manifest = manifest.with_payloads(&artifacts)?;
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
        let entry = match (manifest.architecture, manifest.summary) {
            (None, None) => CatalogEntry::Portable(sha256),
            (architecture, summary) => CatalogEntry::Metadata {
                manifest: sha256,
                architecture,
                summary,
            },
        };
        catalog.entries.insert(id.clone(), entry);
        false
    };
    if let Some(evidence) = evidence {
        if let Some(manifest) = &evidence.published_manifest {
            ensure!(
                catalog.entries[&id].manifest_digest() == manifest.sha256,
                "build attestation names a different published manifest for {id}"
            );
        }
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
            rebuild::archive(repo, catalog, &id, &evidence)?
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

pub(crate) fn read_file_index(repo: &Path, hash: &str) -> Result<FileIndex> {
    let bytes = read_object(repo, FILE_INDEX_OBJECTS, hash, FILE_INDEX_LIMIT)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Enable a signed file lookup index for this publisher's catalogs.
pub fn enable_file_index(repo: &Path, publisher: &str, key: &Path) -> Result<String> {
    validate_publisher(publisher)?;
    let key = read_signing_key(key)?;
    let _lock = lock_repo(repo)?;
    let hash = publication_base(repo)?.context("repository has no catalog to index")?;
    let mut catalog = publication_catalog(repo, publisher, &key, Some(&hash))?;
    if let Some(index) = &catalog.file_index {
        read_file_index(repo, index)?;
        return Ok(hash);
    }
    prepare_objects(repo)?;
    catalog.file_index = Some(store_file_index(repo, &catalog)?);
    write_catalog_document(repo, catalog, &key)
}

fn store_file_index(repo: &Path, catalog: &Catalog) -> Result<String> {
    let mut index = FileIndex::new();
    for (id, entry) in &catalog.entries {
        let package = read_package(repo, &catalog.publisher, id, entry)?;
        for action in &package.manifest.action {
            let path = action.path();
            let basename = path.rsplit('/').next().unwrap_or(path);
            index
                .entry(basename.to_owned())
                .or_default()
                .insert(id.clone());
        }
    }
    let bytes = serde_json::to_vec(&index)?;
    ensure!(
        bytes.len() as u64 <= FILE_INDEX_LIMIT,
        "file index exceeds {FILE_INDEX_LIMIT} bytes"
    );
    store(repo, FILE_INDEX_OBJECTS, &bytes)
}

fn write_catalog(repo: &Path, mut catalog: Catalog, key: &Ed25519KeyPair) -> Result<String> {
    if catalog.file_index.is_some() {
        catalog.file_index = Some(store_file_index(repo, &catalog)?);
    }
    write_catalog_document(repo, catalog, key)
}

fn write_catalog_document(
    repo: &Path,
    mut catalog: Catalog,
    key: &Ed25519KeyPair,
) -> Result<String> {
    catalog.rename_targets()?;
    catalog.check_source_archives()?;
    catalog.sequence = Some(catalog.next_sequence()?);
    let signer = keys::signer(
        &catalog.publisher,
        catalog.sequence,
        &catalog.key_rotations,
        keys::Trust::Current(key.public_key().as_ref()),
    )?;
    catalog.validate_profile_build_trust(&signer)?;
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
        ensure!(
            !self.verify_toolchains || build.published_manifest.is_some(),
            "toolchain verification policy requires version 3 build evidence"
        );
        ensure!(
            !self.archive_build_inputs || build.published_manifest.is_some(),
            "archiving rebuild inputs requires version 3 build evidence"
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
        let anchor = catalog
            .key_rotations
            .first()
            .map_or(key, |rotation| rotation.previous_key());
        let release_profiles = profiles::check(repo, catalog, current, &policy.profiles, anchor)?;
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
        let signer = read_signing_key(key)?;
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

    #[test]
    fn companion_lineage_guards_primary_successors_and_matches_the_companion() -> Result<()> {
        let repo = tempfile::tempdir()?;
        prepare_objects(repo.path())?;
        let key = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let signer = Ed25519KeyPair::from_pkcs8(key.as_ref()).unwrap();
        let anchor = signer.public_key().as_ref();
        let pin = CatalogPin {
            publisher: "seed".to_owned(),
            sha256: write_catalog(repo.path(), Catalog::empty("seed"), &signer)?,
            build_trust: None,
            lineage: Some(CatalogLineage {
                anchor_sha256: digest(anchor),
                sequence: 1,
            }),
        };
        pin.read(repo.path(), "seed", anchor)?;

        let mut previous = Catalog::empty("canopa");
        previous.profiles_required = true;
        previous.sequence = NonZeroU64::new(1);
        previous.release_profiles.insert(
            "desktop".to_owned(),
            ReleaseProfile {
                architecture: "x86_64".to_owned(),
                facets: BTreeMap::new(),
                companions: vec![pin.clone()],
                roots: BTreeMap::new(),
                composition: BTreeMap::new(),
                elf_library_dirs: vec![],
                elf_symbol_versions: false,
                rollout_percentage: None,
            },
        );
        let previous_hash = digest(b"previous primary catalog");
        let mut next = previous.clone();
        next.sequence = NonZeroU64::new(2);
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
        update.lineage.as_mut().unwrap().anchor_sha256 = digest(b"another key");
        changed_pin(update, "release changes companion trust anchor");
        let mut update = pin;
        update.lineage.as_mut().unwrap().sequence = 2;
        let error = update.read(repo.path(), "seed", anchor).unwrap_err();
        assert!(format!("{error:#}").contains("signed companion lineage differs"));
        changed_pin(update, "companion catalog rollback or fork");
        Ok(())
    }

    #[test]
    fn legacy_catalog_signatures_and_first_sequence() -> Result<()> {
        let repo = tempfile::tempdir()?;
        prepare_objects(repo.path())?;
        let key =
            Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).expect("generate key");
        let key = Ed25519KeyPair::from_pkcs8(key.as_ref()).expect("read generated key");
        let portable = digest(b"portable");
        let targeted = digest(b"targeted");
        let legacy = format!(
            r#"{{"format":1,"publisher":"canopa","entries":{{"cps://canopa/test/portable@1.0.0-0":"{portable}","cps://canopa/test/targeted@1.0.0-0":{{"manifest":"{targeted}","architecture":"x86_64"}}}}}}"#
        );
        let signed = format!(
            r#"{{"catalog":{legacy},"signature":"{}"}}"#,
            hex::encode(key.sign(legacy.as_bytes()).as_ref())
        );
        let hash = store(repo.path(), "catalogs", signed.as_bytes())?;
        let read =
            |hash: &str| read_catalog(repo.path(), hash, "canopa", key.public_key().as_ref());
        let previous = read(&hash)?;
        assert!(previous.sequence.is_none());
        write_catalog(repo.path(), previous.clone(), &key)?;
        let next = read(&read_current(repo.path())?)?;
        assert_eq!(next.sequence, NonZeroU64::new(1));
        previous.check_successor(&next, &hash)?;
        assert!(
            next.check_successor(&previous, &read_current(repo.path())?)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn catalog_successors_preserve_signed_renames() -> Result<()> {
        let mut previous = Catalog::empty("canopa");
        let old = "cps://canopa/apps/old@1.0.0-0";
        let new = "cps://canopa/apps/new@1.0.0-0";
        for id in [old, new] {
            previous
                .entries
                .insert(id.to_owned(), CatalogEntry::Portable(digest(id.as_bytes())));
        }
        previous.rename("cps://canopa/apps/old@2.0.0-0", "apps/new".into())?;
        previous.sequence = NonZeroU64::new(1);
        let base = digest(b"previous catalog");
        let mut next = previous.clone();
        next.sequence = NonZeroU64::new(2);
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
        let manifest = CatalogEntry::Portable(digest(b"active manifest"));
        let attestation = digest(b"build evidence");
        let previous = Catalog {
            format: FORMAT,
            publisher: "canopa".to_owned(),
            entries: BTreeMap::from([(active.clone(), manifest.clone())]),
            file_index: None,
            attestations: BTreeMap::from([(active.clone(), attestation.clone())]),
            attestations_required: true,
            toolchains_verified: true,
            build_inputs: BTreeMap::from([(
                active.clone(),
                serde_json::from_value(serde_json::json!({
                    "recipe": digest(b"reviewed recipe"),
                    "manifest": digest(b"reviewed manifest"),
                }))?,
            )]),
            source_archives_required: false,
            withdrawals: BTreeMap::from([(
                withdrawn.clone(),
                Withdrawal {
                    manifest: CatalogEntry::Portable(digest(b"withdrawn manifest")),
                    attestation: Some(digest(b"withdrawn build evidence")),
                },
            )]),
            sequence: NonZeroU64::new(1),
            profiles_required: true,
            obsoletions: BTreeMap::from([("desktop/retired".to_owned(), "2.0.0-0".parse()?)]),
            renames: BTreeMap::new(),
            key_rotations: Vec::new(),
            release_profiles: BTreeMap::from([(
                "desktop".to_owned(),
                ReleaseProfile {
                    architecture: "x86_64".to_owned(),
                    facets: BTreeMap::new(),
                    companions: Vec::new(),
                    roots: BTreeMap::new(),
                    composition: BTreeMap::new(),
                    elf_library_dirs: vec!["/usr/lib".to_owned()],
                    elf_symbol_versions: true,
                    rollout_percentage: None,
                },
            )]),
        };
        let mut next = previous.clone();
        next.sequence = NonZeroU64::new(3);
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
