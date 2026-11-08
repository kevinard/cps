use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::catalogs::requested;
use crate::image::{Image, Installed, State};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum MarkChoice {
    Explicit,
    Automatic,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IntentChange {
    pub package: String,
    pub requested: bool,
}

pub(super) fn mark(
    image: &Image,
    state: &mut State,
    choice: MarkChoice,
    packages: &[String],
) -> Result<(BTreeMap<String, Installed>, Vec<IntentChange>)> {
    ensure!(
        crate::verify::manifest_paths(state)? == state.paths,
        "path ownership index differs from installed manifests"
    );
    image.verify_catalog_archives(state)?;
    let mut intent = Image::profile_intent(state);
    let mut installed = state.installed.clone();
    let mut changes = Vec::new();
    for (name, spec) in requested(state, packages)? {
        let entry = installed
            .get_mut(name)
            .with_context(|| format!("package is not installed: {name}"))?;
        let manifest = &entry.published.manifest;
        ensure!(
            spec.version
                .is_none_or(|version| version == manifest.version),
            "installed package does not match the requested version: {}",
            manifest.id()
        );
        let changed = match choice {
            MarkChoice::Explicit => intent.local.insert(name.to_owned()),
            MarkChoice::Automatic => intent.local.remove(name),
        };
        if changed {
            let requested = choice == MarkChoice::Explicit;
            entry.explicit = requested || intent.roots.contains(name);
            changes.push(IntentChange {
                package: manifest.id(),
                requested,
            });
        }
    }
    if state.settings.profile_intent.is_some() {
        state.settings.profile_intent = Some(intent);
    }
    state.settings.check_profile_intent(&installed)?;
    Ok((installed, changes))
}
