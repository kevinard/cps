use std::{collections::BTreeMap, fmt};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{catalogs::Catalogs, freeze};
use crate::{
    image::{Image, State},
    model::{Version, VersionConstraint},
    repo::{CatalogPin, ReleaseAdvisory, ReleaseProfile},
};

/// The signed target profile is outside this image's rollout cohort.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PhasedUpdateDeferred {
    pub publisher: String,
    pub profile: String,
    pub catalog: String,
    pub percentage: u8,
    pub cohort: u8,
}

pub(super) struct Rollout<'a> {
    pub force: bool,
    pub advisories: Option<&'a mut Vec<ReleaseAdvisory>>,
}

pub(super) fn matching_advisories(
    state: &State,
    profile: &ReleaseProfile,
) -> Result<Vec<ReleaseAdvisory>> {
    let mut matching = Vec::new();
    for advisory in &profile.advisories {
        if let Some(installed) = state.installed.get(&advisory.package) {
            let manifest = &installed.published.manifest;
            if manifest.publisher == advisory.publisher
                && VersionConstraint::parse(Some(&advisory.affected))?.matches(manifest.version)
            {
                matching.push(advisory.clone());
            }
        }
    }
    Ok(matching)
}

impl fmt::Display for PhasedUpdateDeferred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Profile {}/{} upgrade deferred at {}% rollout (cohort {}). Use --force-phased-update to proceed.",
            self.publisher, self.profile, self.percentage, self.cohort
        )
    }
}

impl std::error::Error for PhasedUpdateDeferred {}

pub(super) fn check_rollout(
    image: &Image,
    state: &State,
    pin: &CatalogPin,
    profile: &ReleaseProfile,
) -> Result<()> {
    let Some(percentage) = profile
        .rollout_percentage
        .filter(|&percentage| percentage < 100)
    else {
        return Ok(());
    };
    if state.installed.is_empty() || profile.composition == state.settings.profile_packages {
        return Ok(());
    }
    let name = state
        .publishers
        .release_profile()
        .context("missing profile name")?;
    let companions: Vec<_> = profile
        .companions
        .iter()
        .map(|companion| (&companion.publisher, &companion.sha256))
        .collect();
    // The percentage and primary catalog digest can change when a rollout widens.
    let key = serde_json::to_vec(&(
        "cps-profile-rollout-v1",
        &pin.publisher,
        name,
        &profile.roots,
        &profile.composition,
        companions,
        image.rollout_identity(&state.settings)?,
    ))?;
    let hash = Sha256::digest(key);
    let cohort = (u64::from_be_bytes(hash[..8].try_into()?) % 100) as u8;
    if cohort >= percentage {
        return Err(PhasedUpdateDeferred {
            publisher: pin.publisher.clone(),
            profile: name.to_owned(),
            catalog: pin.sha256.clone(),
            percentage,
            cohort,
        }
        .into());
    }
    Ok(())
}

/// A newer catalog version outside the actionable upgrade plan.
#[derive(Debug, Serialize)]
pub struct HeldBackUpdate {
    pub name: String,
    pub publisher: String,
    pub installed: Version,
    pub available: Version,
    pub selected: Option<Version>,
    pub reason: String,
}

pub(super) fn candidates(
    state: &State,
    catalogs: &Catalogs,
    update_profile: bool,
) -> Vec<HeldBackUpdate> {
    let composition = if update_profile {
        &catalogs
            .profile
            .as_ref()
            .expect("selected profile")
            .composition
    } else {
        &state.settings.profile_packages
    };
    let mut latest = BTreeMap::new();
    for package in &catalogs.candidates.packages {
        let manifest = &package.manifest;
        let Some(installed) = state.installed.get(&manifest.name) else {
            continue;
        };
        let installed = &installed.published.manifest;
        if manifest.version > installed.version
            && composition
                .get(&manifest.name)
                .is_none_or(|digest| digest == &package.sha256)
        {
            let version = latest.entry(&manifest.name).or_insert(manifest.version);
            *version = (*version).max(manifest.version);
        }
    }
    latest
        .into_iter()
        .map(|(name, available)| {
            let manifest = &state.installed[name].published.manifest;
            let reason = match state.settings.freezes.get(name) {
                Some(frozen) => format!(
                    "{name} is frozen at {}{}",
                    frozen.version,
                    freeze::reason_suffix(frozen)
                ),
                None => "Current upgrade constraints kept this version out of the plan".into(),
            };
            HeldBackUpdate {
                name: name.clone(),
                publisher: manifest.publisher.clone(),
                installed: manifest.version,
                available,
                selected: None,
                reason,
            }
        })
        .collect()
}
