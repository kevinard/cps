use std::{
    collections::BTreeMap,
    iter,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

use super::{Image, Publisher, Publishers, Settings, State, builds, catalogs};
use crate::{
    attestation::BuildTrust,
    kdl_format::read_build_trust,
    model::validate_publisher,
    repo::{
        BuildAudit, BuildReceipt, CatalogCheckpoint, CatalogPin, Published, ReleaseProfile,
        audit_installed_builds, catalog_index, release_profile,
    },
};

pub struct PublisherInfo {
    pub name: String,
    pub repository: PathBuf,
}

pub(crate) struct SelectedProfile {
    primary: CatalogPin,
    pub record: ReleaseProfile,
}

impl SelectedProfile {
    pub(crate) fn pin(&self, publisher: &str) -> Option<&CatalogPin> {
        iter::once(&self.primary)
            .chain(&self.record.companions)
            .find(|pin| pin.publisher == publisher)
    }
}

impl Image {
    pub(crate) fn select_profile(
        &self,
        publishers: &Publishers,
        settings: &Settings,
        primary_pin: Option<&str>,
    ) -> Result<Option<SelectedProfile>> {
        let primary = &publishers.as_slice()[0];
        let checkpoint = settings.catalog_checkpoints.get(&primary.name);
        let selected = publishers.select_profile(
            &settings.arch,
            &settings.facets,
            settings.bootstrap_catalog.as_deref().or(primary_pin),
            checkpoint,
        )?;
        if let Some(selected) = &selected {
            self.check_catalog_transition(
                settings,
                primary,
                &selected.primary.sha256,
                settings.archived_profile_catalog.as_deref(),
            )?;
        }
        Ok(selected)
    }

    pub(crate) fn release_pins(
        &self,
        publishers: &Publishers,
        settings: &Settings,
    ) -> Result<BTreeMap<String, CatalogPin>> {
        let Some(selected) = self.select_profile(publishers, settings, None)? else {
            return Ok(BTreeMap::new());
        };
        Ok(iter::once(selected.primary)
            .chain(selected.record.companions)
            .map(|pin| (pin.publisher.clone(), pin))
            .collect())
    }
}

impl Publishers {
    pub(crate) fn select_profile(
        &self,
        arch: &str,
        facets: &BTreeMap<String, bool>,
        primary_pin: Option<&str>,
        previous: Option<&CatalogCheckpoint>,
    ) -> Result<Option<SelectedProfile>> {
        let Some(name) = self.release_profile() else {
            return Ok(None);
        };
        let (primary, companions) = self
            .as_slice()
            .split_first()
            .context("missing primary publisher")?;
        let (checkpoint, profile) = release_profile(
            &primary.repo,
            &primary.name,
            &primary.key,
            primary_pin,
            name,
        )?;
        if let Some(previous) = previous {
            previous.check(&checkpoint).with_context(|| {
                format!("checking installed catalog checkpoint for {}", primary.name)
            })?;
        }
        profile.check_image(
            name,
            arch,
            facets,
            companions.iter().map(|publisher| publisher.name.as_str()),
        )?;
        Ok(Some(SelectedProfile {
            primary: CatalogPin::primary(primary.name.clone(), checkpoint.catalog),
            record: profile,
        }))
    }
}

pub fn list_publishers(root: &Path) -> Result<Vec<PublisherInfo>> {
    let image = Image::open_query(root)?;
    image.require_clean()?;
    Ok(image
        .publishers()?
        .as_slice()
        .iter()
        .map(|publisher| PublisherInfo {
            name: publisher.name.clone(),
            repository: publisher.repo.clone(),
        })
        .collect())
}

pub fn add_publisher(root: &Path, name: &str, repo: &Path, key: &Path) -> Result<()> {
    add_publisher_with_build_trust(root, name, repo, key, None)
}

pub fn add_publisher_with_build_trust(
    root: &Path,
    name: &str,
    repo: &Path,
    key: &Path,
    policy: Option<&Path>,
) -> Result<()> {
    let mut image = Image::open(root, true)?;
    let configured = editable_publishers(&image)?;
    ensure!(
        configured.get(name).is_none(),
        "publisher {name} is already configured"
    );
    let mut publisher = Publisher::from_config(name, repo, key)?;
    if let Some(path) = policy {
        let trust = read_build_trust(path)?.load(&publisher.key)?;
        audit_installed_builds(
            &publisher.repo,
            name,
            &publisher.key,
            None,
            |_| Ok(()),
            &trust,
            [],
        )?;
        publisher.build_trust = Some(trust);
    } else {
        catalog_index(&publisher.repo, name, &publisher.key, None)
            .with_context(|| format!("verifying signed catalog for {name}"))?;
    }
    let mut publishers = configured.as_slice().to_vec();
    publishers.push(publisher);
    save(&mut image, Publishers::new(publishers, None)?, &[])
}

/// Replace the locally provisioned builder policy. Audit installed packages
/// first unless the administrator explicitly scopes the change to future plans.
pub fn set_publisher_build_trust(
    root: &Path,
    name: &str,
    policy: &Path,
    future_only: bool,
) -> Result<()> {
    let mut image = Image::open(root, true)?;
    image.require_clean()?;
    let state = image.state()?;
    let mut publishers = state.publishers.as_slice().to_vec();
    let publisher = &mut publishers[position(state.publishers.as_slice(), name)?];
    let trust = read_build_trust(policy)?.load(&publisher.key)?;
    let receipts = if future_only {
        Vec::new()
    } else {
        let profile = catalogs::read_profile(&image.db, &state)?;
        let audit = audit(&image, &state, publisher, profile.as_ref(), &trust, true)?;
        ensure!(
            audit.failures.is_empty(),
            "installed build audit failed for {name}\n{}",
            audit.failures.join("\n")
        );
        audit.receipts
    };
    if publisher.build_trust.as_ref() != Some(&trust) {
        publisher.build_trust = Some(trust);
        let profile = state.publishers.release_profile().map(str::to_owned);
        save(&mut image, Publishers::new(publishers, profile)?, &receipts)?;
    } else if !receipts.is_empty() {
        let tx = image.db.transaction()?;
        builds::save_audit(&tx, &state.publishers, &state.settings, &receipts)?;
        tx.commit()?;
    }
    Ok(())
}

pub fn audit_publisher_builds(root: &Path, name: &str) -> Result<BuildAudit> {
    let image = Image::open_query(root)?;
    image.require_clean()?;
    let state = image.state()?;
    let publisher = state
        .publishers
        .get(name)
        .with_context(|| format!("publisher {name} is not configured"))?;
    let profile = catalogs::read_profile(&image.db, &state)?;
    if let Some(signed) = profile
        .as_ref()
        .and_then(|profile| profile.companions.iter().find(|pin| pin.publisher == name))
        .and_then(|pin| pin.build_trust.as_ref())
    {
        let mut policies = vec![signed];
        if let Some(local) = publisher
            .build_trust
            .as_ref()
            .filter(|local| *local != signed)
        {
            policies.push(local);
        }
        let (mut audit, missing) =
            audit_archived(&image, &state, publisher, profile.as_ref(), &policies)?;
        audit.failures.extend(missing.into_iter().map(|package| {
            format!(
                "missing archived build evidence for {}",
                package.manifest.id()
            )
        }));
        return Ok(audit);
    }
    let trust = publisher.build_trust.as_ref().with_context(|| {
        format!("publisher {name} has no local or archived signed build trust policy")
    })?;
    audit(&image, &state, publisher, profile.as_ref(), trust, false)
}

fn audit(
    image: &Image,
    state: &State,
    publisher: &Publisher,
    profile: Option<&ReleaseProfile>,
    trust: &BuildTrust,
    check_current: bool,
) -> Result<BuildAudit> {
    let (mut audit, missing) = audit_archived(image, state, publisher, profile, &[trust])?;
    if check_current || !missing.is_empty() {
        let repository_audit = || {
            let pins = image.release_pins(&state.publishers, &state.settings)?;
            audit_installed_builds(
                &publisher.repo,
                &publisher.name,
                &publisher.key,
                pins.get(&publisher.name),
                |next| image.check_catalog(&state.publishers, &state.settings, publisher, next),
                trust,
                missing.iter().copied(),
            )
        };
        match repository_audit() {
            Ok(found) => {
                audit.verified += found.verified;
                audit.failures.extend(found.failures);
                audit.receipts = found.receipts;
            }
            Err(error) if check_current => return Err(error),
            Err(error) => audit.failures.extend(missing.iter().map(|package| {
                format!(
                    "checking repository build evidence for {}: {error:#}",
                    package.manifest.id()
                )
            })),
        }
    }
    Ok(audit)
}

fn audit_archived<'a>(
    image: &Image,
    state: &'a State,
    publisher: &Publisher,
    profile: Option<&ReleaseProfile>,
    policies: &[&BuildTrust],
) -> Result<(BuildAudit, Vec<&'a Published>)> {
    let mut audit = BuildAudit::default();
    let mut installed = state
        .installed
        .values()
        .map(|package| &package.published)
        .filter(|package| package.manifest.publisher == publisher.name)
        .peekable();
    let mut missing = Vec::new();
    if installed.peek().is_none() {
        return Ok((audit, missing));
    }
    let applied = catalogs::read_applied(&image.db, state, publisher, profile)?;
    ensure!(
        !state.settings.catalog_archives || applied.is_some(),
        "missing catalog checkpoint for installed publisher {}",
        publisher.name
    );
    let archive = builds::exists(&image.db)?;
    let mut supplemental = None;
    for package in installed {
        if !archive {
            missing.push(package);
            continue;
        }
        let result = match builds::read(&image.db, &package.sha256) {
            Ok(None) => {
                missing.push(package);
                continue;
            }
            Ok(Some((receipt, bytes))) => (|| {
                let admitted = match &applied {
                    Some((catalog, _)) => catalog
                        .check_installed(package)?
                        .map(|hash| (catalog, hash)),
                    None => None,
                };
                let (catalog, expected) = if let Some(admitted) = admitted {
                    admitted
                } else {
                    if supplemental.is_none() {
                        let found = builds::admission_catalog(&image.db, publisher)?;
                        if let (Some((applied, applied_hash)), Some((catalog, hash))) =
                            (&applied, &found)
                            && hash != applied_hash
                        {
                            applied.check_successor(catalog, applied_hash).context(
                                "checking archived build admission against the applied catalog",
                            )?;
                        }
                        supplemental = Some(found);
                    }
                    let Some((catalog, _)) = supplemental.as_ref().and_then(Option::as_ref) else {
                        if applied.is_some() {
                            // Re-audit old receipts whose later catalog was never saved.
                            return Ok(false);
                        }
                        // Legacy images can have receipts without their full catalog.
                        policies.iter().try_for_each(|trust| {
                            receipt.verify(&bytes, package, &publisher.key, trust)
                        })?;
                        return Ok(true);
                    };
                    let hash = catalog
                        .check_installed(package)?
                        .context("no signed catalog admission for archived build attestation")?;
                    (catalog, hash)
                };
                receipt.check_attestation(expected)?;
                let publisher_keys = catalog.publisher_keys(&publisher.key)?;
                policies.iter().try_for_each(|trust| {
                    for key in &publisher_keys {
                        trust.check_publisher_key(key)?;
                    }
                    receipt.verify(&bytes, package, &publisher.key, trust)
                })?;
                Ok(true)
            })(),
            Err(error) => Err(error),
        };
        match result {
            Ok(true) => audit.verified += 1,
            Ok(false) => missing.push(package),
            Err(error) => audit.failures.push(format!(
                "checking archived build evidence for {}: {error:#}",
                package.manifest.id()
            )),
        }
    }
    Ok((audit, missing))
}

pub fn prefer_publisher(root: &Path, name: &str) -> Result<()> {
    let mut image = Image::open(root, true)?;
    let configured = editable_publishers(&image)?;
    let mut publishers = configured.as_slice().to_vec();
    let position = position(&publishers, name)?;
    if position == 0 {
        return Ok(());
    }
    publishers[..=position].rotate_right(1);
    save(&mut image, Publishers::new(publishers, None)?, &[])
}

pub fn remove_publisher(root: &Path, name: &str) -> Result<()> {
    let mut image = Image::open(root, true)?;
    let configured = editable_publishers(&image)?;
    let mut publishers = configured.as_slice().to_vec();
    let position = position(&publishers, name)?;
    ensure!(
        publishers.len() > 1,
        "cannot remove the image's last publisher"
    );
    for (package_name, package) in image.installed()? {
        package.verify(&package_name, &configured)?;
        ensure!(
            package.published.manifest.publisher != name,
            "cannot remove publisher {name} while {package_name} is installed"
        );
    }
    publishers.remove(position);
    save(&mut image, Publishers::new(publishers, None)?, &[])
}

fn position(publishers: &[Publisher], name: &str) -> Result<usize> {
    validate_publisher(name)?;
    publishers
        .iter()
        .position(|publisher| publisher.name == name)
        .with_context(|| format!("publisher {name} is not configured"))
}

fn editable_publishers(image: &Image) -> Result<Publishers> {
    image.require_clean()?;
    let publishers = image.publishers()?;
    ensure!(
        publishers.release_profile().is_none(),
        "the signed release profile fixes this image's publishers and their order"
    );
    Ok(publishers)
}

fn save(image: &mut Image, publishers: Publishers, receipts: &[BuildReceipt]) -> Result<()> {
    let mut settings = image.settings()?;
    settings
        .catalog_checkpoints
        .retain(|name, _| publishers.get(name).is_some());
    settings.format = publishers.image_format(&settings);
    settings.publisher_generation = settings
        .publisher_generation
        .checked_add(1)
        .context("publisher generation exhausted")?;
    let tx = image.db.transaction()?;
    builds::save_audit(&tx, &publishers, &settings, receipts)?;
    builds::retain_admission(&tx, &publishers)?;
    catalogs::retain(&tx, &settings)?;
    tx.execute(
        "UPDATE publishers SET data=?1 WHERE id=1",
        [serde_json::to_string(&publishers)?],
    )?;
    tx.execute(
        "UPDATE image SET data=?1 WHERE id=1",
        [serde_json::to_string(&settings)?],
    )?;
    tx.commit()?;
    Ok(())
}
