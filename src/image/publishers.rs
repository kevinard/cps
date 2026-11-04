use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

use super::{Image, Publisher, Publishers, State};
use crate::{
    attestation::BuildTrust,
    kdl_format::read_build_trust,
    model::validate_publisher,
    repo::{BuildAudit, audit_installed_builds, catalog_index, release_profile},
};

pub struct PublisherInfo {
    pub name: String,
    pub repository: PathBuf,
}

impl Publishers {
    pub(crate) fn release_pins(
        &self,
        arch: &str,
        facets: &BTreeMap<String, bool>,
        primary_pin: Option<&str>,
    ) -> Result<BTreeMap<String, String>> {
        let Some(name) = self.release_profile() else {
            return Ok(BTreeMap::new());
        };
        let (primary, companions) = self
            .as_slice()
            .split_first()
            .context("missing primary publisher")?;
        let (hash, profile) = release_profile(
            &primary.repo,
            &primary.name,
            &primary.key,
            primary_pin,
            name,
        )?;
        ensure!(
            profile.architecture == arch && &profile.facets == facets,
            "image architecture or facets differ from signed release profile {name}"
        );
        ensure!(
            companions
                .iter()
                .map(|publisher| &publisher.name)
                .eq(profile.companions.iter().map(|pin| &pin.publisher)),
            "image publisher order differs from signed release profile {name}"
        );
        let mut pins = BTreeMap::from([(primary.name.clone(), hash)]);
        pins.extend(
            profile
                .companions
                .into_iter()
                .map(|pin| (pin.publisher, pin.sha256)),
        );
        Ok(pins)
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
        audit_installed_builds(&publisher.repo, name, &publisher.key, None, &trust, [])?;
        publisher.build_trust = Some(trust);
    } else {
        catalog_index(&publisher.repo, name, &publisher.key, None)
            .with_context(|| format!("verifying signed catalog for {name}"))?;
    }
    let mut publishers = configured.as_slice().to_vec();
    publishers.push(publisher);
    save(&mut image, Publishers::new(publishers, None)?)
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
    if !future_only {
        let audit = audit(&state, publisher, &trust)?;
        ensure!(
            audit.failures.is_empty(),
            "installed build audit failed for {name}\n{}",
            audit.failures.join("\n")
        );
    }
    if publisher.build_trust.as_ref() != Some(&trust) {
        publisher.build_trust = Some(trust);
        let profile = state.publishers.release_profile().map(str::to_owned);
        save(&mut image, Publishers::new(publishers, profile)?)?;
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
    let trust = publisher
        .build_trust
        .as_ref()
        .with_context(|| format!("publisher {name} has no build trust policy"))?;
    audit(&state, publisher, trust)
}

fn audit(state: &State, publisher: &Publisher, trust: &BuildTrust) -> Result<BuildAudit> {
    let pins = state
        .publishers
        .release_pins(&state.settings.arch, &state.settings.facets, None)?;
    let installed = state
        .installed
        .values()
        .map(|package| &package.published)
        .filter(|package| package.manifest.publisher == publisher.name);
    audit_installed_builds(
        &publisher.repo,
        &publisher.name,
        &publisher.key,
        pins.get(&publisher.name).map(String::as_str),
        trust,
        installed,
    )
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
    save(&mut image, Publishers::new(publishers, None)?)
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
    save(&mut image, Publishers::new(publishers, None)?)
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

fn save(image: &mut Image, publishers: Publishers) -> Result<()> {
    let mut settings = image.settings()?;
    settings.format = publishers.image_format();
    settings.publisher_generation = settings
        .publisher_generation
        .checked_add(1)
        .context("publisher generation exhausted")?;
    let tx = image.db.transaction()?;
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
