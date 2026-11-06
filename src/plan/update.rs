use std::collections::BTreeMap;

use serde::Serialize;

use super::{catalogs::Catalogs, freeze};
use crate::{image::State, model::Version};

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
