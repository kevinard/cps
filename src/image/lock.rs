use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{RenameFlags, renameat_with};
use serde::{Deserialize, Serialize};

use super::{Image, Installed, Publisher, Publishers, State, catalogs};
use crate::{
    kdl_format::ImageDefinition,
    model::{
        ImagePolicy, Node, PackageFacets, PackageSpec, Selection, Version, digest, validate_digest,
        validate_name, validate_policy, validate_publisher,
    },
    query::verified_intent,
    repo::{CATALOG_LIMIT, Published, catalog_index, export_locked_repository, read_limited},
};

const LOCK_FORMAT: u32 = 1;
const BUNDLE_LOCK: &str = "image.lock.json";
const BUNDLE_REPOSITORIES: &str = "repositories";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImageLock {
    format: u32,
    kind: LockKind,
    pub(crate) architecture: String,
    pub(crate) facets: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) package_facets: PackageFacets,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) variants: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) mediators: BTreeMap<String, String>,
    pub(crate) profile: Option<String>,
    pub(crate) signed_publishers: usize,
    pub(crate) publishers: Vec<LockedPublisher>,
    pub(crate) local_roots: BTreeSet<String>,
    packages: BTreeMap<String, LockedPackage>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum LockKind {
    ImageLock,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LockedPublisher {
    pub name: String,
    pub catalog: String,
    key_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct LockedPackage {
    publisher: String,
    version: Version,
    manifest_sha256: String,
}

/// Capture the installed package composition and its signed catalog pins.
pub fn capture_image_lock(root: &Path) -> Result<ImageLock> {
    let image = Image::open_query(root)?;
    capture(&image).map(|(lock, _)| lock)
}

fn capture(image: &Image) -> Result<(ImageLock, State)> {
    let (state, intent) = verified_intent(image)?;
    let profile = state.publishers.release_profile();
    if profile.is_some() {
        let applied = catalogs::read_profile(&image.db, &state)?
            .context("missing archived release profile")?;
        ensure!(
            !applied.composition.is_empty()
                && applied.composition == state.settings.profile_packages
                && applied.roots.keys().eq(&intent.roots),
            "the archived profile differs from the applied base; apply the signed profile update before locking"
        );
    }
    let mut publishers = Vec::new();
    for publisher in state.publishers.as_slice() {
        let catalog = if let Some(point) = state.settings.catalog_checkpoints.get(&publisher.name) {
            let archive = catalogs::read_checkpoint(&image.db, publisher, point)?;
            for package in state.installed.values().map(|entry| &entry.published) {
                let manifest = &package.manifest;
                if manifest.publisher == publisher.name {
                    ensure!(
                        archive.is_active(&manifest.id()),
                        "{} is withdrawn from the archived catalog",
                        manifest.id()
                    );
                }
            }
            point.catalog.clone()
        } else {
            // Newly added, unused publishers have yet to acquire a checkpoint.
            catalog_index(&publisher.repo, &publisher.name, &publisher.key, None)?
                .checkpoint
                .catalog
        };
        publishers.push(LockedPublisher {
            name: publisher.name.clone(),
            catalog,
            key_sha256: digest(&publisher.key),
        });
    }
    let packages = state
        .installed
        .iter()
        .map(|(name, entry)| {
            let manifest = &entry.published.manifest;
            (
                name.clone(),
                LockedPackage {
                    publisher: manifest.publisher.clone(),
                    version: manifest.version.clone(),
                    manifest_sha256: entry.published.sha256.clone(),
                },
            )
        })
        .collect();
    let lock = ImageLock {
        format: LOCK_FORMAT,
        kind: LockKind::ImageLock,
        architecture: state.settings.arch.clone(),
        facets: state.settings.facets.clone(),
        package_facets: state.settings.package_facets.clone(),
        variants: state.settings.variants.clone(),
        mediators: state.settings.mediators.clone(),
        profile: profile.map(str::to_owned),
        signed_publishers: state.publishers.signed_publishers().len(),
        publishers,
        local_roots: intent.local,
        packages,
    };
    lock.validate()?;
    Ok((lock, state))
}

/// Export the locked composition and the verified objects needed for offline provisioning.
pub fn export_image_bundle(root: &Path, output: &Path) -> Result<()> {
    let image = Image::open_query(root)?;
    let (lock, state) = capture(&image)?;
    ensure!(
        crate::verify::manifest_paths(&state)? == state.paths,
        "path ownership index differs from installed manifests"
    );
    let name = output.file_name().context("missing bundle output name")?;
    let destination = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stage = tempfile::tempdir_in(destination)?;
    let repositories = stage.path().join(BUNDLE_REPOSITORIES);
    fs::create_dir(&repositories)?;

    let mut payloads = BTreeMap::<_, BTreeMap<&str, &Node>>::new();
    for managed in state.paths.values() {
        if let Node::File { sha256, size, .. } = &managed.node {
            let owner = managed
                .owners
                .first()
                .context("managed file has no package owner")?;
            let publisher = state.installed[owner].published.manifest.publisher.as_str();
            let blobs = payloads.entry(publisher).or_default();
            if let Some(Node::File { size: previous, .. }) = blobs.insert(sha256, &managed.node) {
                ensure!(previous == size, "conflicting payload lengths for {sha256}");
            }
        }
    }
    for publisher in state.publishers.as_slice() {
        let name = publisher.name.as_str();
        let packages = state
            .installed
            .values()
            .map(|entry| &entry.published)
            .filter(|package| package.manifest.publisher == name)
            .collect::<Vec<_>>();
        export_locked_repository(
            publisher,
            lock.catalog(name)?,
            &packages,
            &payloads.remove(name).unwrap_or_default(),
            &repositories.join(name),
        )
        .with_context(|| format!("exporting locked repository {name}"))?;
    }
    File::open(&repositories)?.sync_all()?;
    let mut file = File::create_new(stage.path().join(BUNDLE_LOCK))?;
    serde_json::to_writer_pretty(&mut file, &lock)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    File::open(stage.path())?.sync_all()?;
    let parent = File::open(destination)?;
    renameat_with(
        &parent,
        stage.path().file_name().context("missing staging name")?,
        &parent,
        name,
        RenameFlags::NOREPLACE,
    )?;
    parent.sync_all()?;
    Ok(())
}

pub fn read_image_lock(path: &Path) -> Result<ImageLock> {
    let load = || -> Result<ImageLock> {
        let bytes = read_limited(File::open(path)?, CATALOG_LIMIT)?;
        let lock: ImageLock = serde_json::from_slice(&bytes)?;
        lock.validate()?;
        Ok(lock)
    };
    load().with_context(|| format!("reading image lock {}", path.display()))
}

impl ImageLock {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(self.format == LOCK_FORMAT, "unsupported image lock format");
        validate_publisher(&self.architecture).context("invalid locked architecture")?;
        validate_policy(&self.facets, &self.package_facets, &self.variants)
            .context("invalid locked facets or variants")?;
        for (group, provider) in &self.mediators {
            validate_publisher(group).context("invalid locked mediator group")?;
            validate_name(provider).context("invalid locked mediator provider")?;
        }
        if let Some(profile) = &self.profile {
            validate_name(profile).context("invalid locked profile")?;
            ensure!(
                (1..=self.publishers.len()).contains(&self.signed_publishers),
                "invalid signed publisher count in image lock"
            );
        } else {
            ensure!(
                self.signed_publishers == 0,
                "image lock has signed publishers without a profile"
            );
        }
        let mut names = BTreeSet::new();
        for publisher in &self.publishers {
            validate_publisher(&publisher.name)?;
            validate_digest(&publisher.catalog)?;
            validate_digest(&publisher.key_sha256)?;
            ensure!(
                names.insert(publisher.name.as_str()),
                "duplicate locked publisher {}",
                publisher.name
            );
        }
        ensure!(!names.is_empty(), "image lock requires publishers");
        for (name, package) in &self.packages {
            validate_name(name)?;
            validate_digest(&package.manifest_sha256)?;
            ensure!(
                names.contains(package.publisher.as_str()),
                "unknown locked publisher for {name}"
            );
        }
        ensure!(
            self.local_roots
                .iter()
                .all(|name| self.packages.contains_key(name)),
            "image lock contains a local request outside the package selection"
        );
        Ok(())
    }

    /// The lock's composition policy. Locked provisioning reproduces it exactly.
    pub(crate) fn policy(&self) -> ImagePolicy {
        ImagePolicy {
            facets: self.facets.clone(),
            package_facets: self.package_facets.clone(),
            variants: self.variants.clone(),
        }
    }

    pub(super) fn check_configuration(
        &self,
        selection: Selection<'_>,
        profile: Option<&str>,
        publishers: &[Publisher],
    ) -> Result<()> {
        ensure!(
            selection.arch == self.architecture
                && selection.facets == &self.facets
                && selection.variants == &self.variants
                && profile == self.profile.as_deref(),
            "architecture, facets, variants or release profile differ from the image lock"
        );
        ensure!(
            selection.packages.is_empty() || selection.packages == &self.package_facets,
            "per-package facet rules differ from the image lock"
        );
        ensure!(
            self.publishers
                .iter()
                .map(|p| p.name.as_str())
                .eq(publishers.iter().map(|p| p.name.as_str())),
            "configured publisher order differs from the image lock"
        );
        for (locked, publisher) in self.publishers.iter().zip(publishers) {
            ensure!(
                digest(&publisher.key) == locked.key_sha256,
                "configured key differs from the image lock for {}",
                publisher.name
            );
        }
        Ok(())
    }

    pub(crate) fn check_target(&self, state: &State) -> Result<()> {
        self.validate()?;
        ensure!(
            state.settings.revision == 0 && state.installed.is_empty() && state.paths.is_empty(),
            "install-lock requires a newly initialized image"
        );
        self.check_configuration(
            state.settings.selection(),
            state.publishers.release_profile(),
            state.publishers.as_slice(),
        )?;
        ensure!(
            state.publishers.signed_publishers().len() == self.signed_publishers,
            "signed publisher order differs from the image lock; initialize with --lock"
        );
        if let Some(pin) = &state.settings.bootstrap_catalog {
            ensure!(
                self.catalog(&self.publishers[0].name)? == pin,
                "bootstrap catalog differs from the image lock"
            );
        }
        Ok(())
    }

    pub(crate) fn catalog(&self, publisher: &str) -> Result<&str> {
        self.publishers
            .iter()
            .find(|p| p.name == publisher)
            .map(|p| p.catalog.as_str())
            .with_context(|| format!("image lock has no catalog for {publisher}"))
    }

    pub(crate) fn retained_catalog(&self, publisher: &str, key: &[u8]) -> Result<&str> {
        let pin = self
            .publishers
            .iter()
            .find(|pin| pin.name == publisher)
            .with_context(|| format!("image lock has no publisher {publisher}"))?;
        ensure!(
            pin.key_sha256 == digest(key),
            "image lock uses a different key for {publisher}"
        );
        Ok(&pin.catalog)
    }

    pub(crate) fn pinned_packages<'a>(
        &'a self,
        publisher: &'a str,
    ) -> impl Iterator<Item = (String, &'a str)> {
        self.packages
            .iter()
            .filter(move |(_, package)| package.publisher == publisher)
            .map(move |(name, package)| {
                (
                    format!("cps://{publisher}/{name}@{}", package.version),
                    package.manifest_sha256.as_str(),
                )
            })
    }

    pub(crate) fn selectors(&self) -> BTreeMap<&str, PackageSpec<'_>> {
        self.packages
            .iter()
            .map(|(name, package)| {
                (
                    name.as_str(),
                    PackageSpec {
                        publisher: Some(&package.publisher),
                        name,
                        version: Some(package.version.clone()),
                    },
                )
            })
            .collect()
    }

    pub(crate) fn replay(&self, candidates: &[Published]) -> Result<BTreeMap<String, Installed>> {
        let mut selected = BTreeMap::new();
        for candidate in candidates {
            let manifest = &candidate.manifest;
            let name = &manifest.name;
            if self.packages.get(name).is_some_and(|locked| {
                locked.version == manifest.version
                    && locked.publisher == manifest.publisher
                    && locked.manifest_sha256 == candidate.sha256
            }) {
                ensure!(
                    selected
                        .insert(
                            name.clone(),
                            Installed {
                                published: candidate.clone(),
                                explicit: self.local_roots.contains(name),
                            }
                        )
                        .is_none(),
                    "duplicate locked package {name}"
                );
            }
        }
        for name in self.packages.keys() {
            ensure!(
                selected.contains_key(name),
                "locked package {name} is unavailable with its exact manifest digest"
            );
        }
        Ok(selected)
    }
}

/// Initialize a fresh root with the local trust configuration and locked profile.
pub fn create_image_from_lock(path: &Path, config: &Path, lock: &ImageLock) -> Result<String> {
    let config = crate::kdl_format::read_image_definition(config)?;
    create_locked(path, config, lock, false)
}

/// Use the bundle's repositories with publisher keys and policy from local configuration.
pub fn create_image_from_bundle(path: &Path, config: &Path, bundle: &Path) -> Result<String> {
    let mut config = crate::kdl_format::read_image_definition(config)?;
    let bundle = fs::canonicalize(bundle)?;
    let lock = read_image_lock(&bundle.join(BUNDLE_LOCK))?;
    for publisher in &mut config.publishers {
        publisher.repository = bundle.join(BUNDLE_REPOSITORIES).join(&publisher.name);
    }
    create_locked(path, config, &lock, true)
}

fn create_locked(
    path: &Path,
    config: ImageDefinition,
    lock: &ImageLock,
    preflight: bool,
) -> Result<String> {
    lock.validate()?;
    ensure!(
        config.image_id.is_none(),
        "locked provisioning assigns a fresh image identity"
    );
    let publishers = super::configured_publishers(&config)?;
    lock.check_configuration(
        config.policy.selection(&config.arch),
        config.release_profile.as_deref(),
        &publishers,
    )?;
    // Per-package facet rules are part of the locked composition; the configuration
    // may omit them, and provisioning then adopts the lock's rules.
    let policy = lock.policy();
    let bootstrap = lock
        .profile
        .as_ref()
        .map(|_| lock.publishers[0].catalog.clone());
    ensure!(
        config.bootstrap_catalog.is_none() || config.bootstrap_catalog == bootstrap,
        "configured bootstrap catalog differs from the image lock"
    );
    let publishers = if let Some(profile) = &lock.profile {
        Publishers::Profiled {
            release_profile: profile.clone(),
            signed_count: (lock.signed_publishers != publishers.len())
                .then_some(lock.signed_publishers),
            publishers,
        }
    } else {
        Publishers::new(publishers, None)?
    };
    publishers.validate()?;
    if let Some(profile) =
        publishers.select_profile(policy.selection(&config.arch), bootstrap.as_deref(), None)?
    {
        ensure!(
            !profile.record.composition.is_empty(),
            "locked release profile has no signed composition"
        );
        for publisher in publishers.signed_publishers() {
            let pin = profile
                .pin(&publisher.name)
                .context("missing signed publisher pin")?;
            ensure!(
                lock.catalog(&publisher.name)? == pin.sha256,
                "locked catalog for {} differs from the signed release profile",
                publisher.name
            );
        }
    }
    if preflight {
        let scratch = tempfile::tempdir()?;
        super::initialize(
            scratch.path(),
            publishers.clone(),
            &config.arch,
            policy.clone(),
            bootstrap.clone(),
            None,
        )?;
        let (plan, state, _) = crate::plan::plan_image(
            scratch.path(),
            crate::Request::InstallLock {
                lock: Box::new(lock.clone()),
            },
            crate::PlanOptions::default(),
            None,
        )
        .context("checking the bundle's locked composition")?;
        crate::apply::validate_cached_payloads(&plan, &state)?;
    }
    super::initialize(path, publishers, &config.arch, policy, bootstrap, None)
}
