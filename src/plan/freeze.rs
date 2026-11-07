use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::Request;
use crate::{
    image::{Image, Installed, State},
    model::{PackageSpec, Version, validate_name},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenPackage {
    pub version: Version,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FreezeChange {
    pub name: String,
    pub from: Option<FrozenPackage>,
    pub to: Option<FrozenPackage>,
}

/// Read committed package freezes without contacting a repository.
pub fn list_freezes(root: &Path) -> Result<BTreeMap<String, FrozenPackage>> {
    let image = Image::open_query(root)?;
    image.require_clean()?;
    Ok(image.state()?.settings.freezes)
}

pub(crate) fn check(
    freezes: &BTreeMap<String, FrozenPackage>,
    installed: &BTreeMap<String, Installed>,
) -> Result<()> {
    for (name, freeze) in freezes {
        validate_name(name).context("invalid frozen package name")?;
        check_reason(freeze.reason.as_deref())?;
        if let Some(package) = installed.get(name) {
            ensure!(
                package.published.manifest.version == freeze.version,
                "{name} is frozen at {}{}; use plan unfreeze before changing its version",
                freeze.version,
                reason_suffix(freeze),
            );
        }
    }
    Ok(())
}

pub(super) fn reason_suffix(freeze: &FrozenPackage) -> String {
    freeze
        .reason
        .as_ref()
        .map_or_else(String::new, |reason| format!(" ({reason:?})"))
}

fn check_reason(reason: Option<&str>) -> Result<()> {
    ensure!(
        reason.is_none_or(|reason| !reason.is_empty()
            && reason.len() <= 4096
            && !reason.contains('\0')),
        "a freeze reason must contain 1 to 4096 bytes without NUL characters"
    );
    Ok(())
}

pub(super) fn changes(
    state: &State,
    request: &Request,
) -> Result<(BTreeMap<String, FrozenPackage>, Vec<FreezeChange>)> {
    let mut next = state.settings.freezes.clone();
    let (packages, reason) = match request {
        Request::Freeze { packages, reason } => {
            check_reason(reason.as_deref())?;
            (packages, Some(reason))
        }
        Request::Unfreeze { packages } => (packages, None),
        _ => return Ok((next, Vec::new())),
    };
    ensure!(!packages.is_empty(), "specify at least one package");
    let mut selectors = BTreeMap::new();
    for package in packages {
        let spec = PackageSpec::parse(package)?;
        if let Some(publisher) = spec.publisher {
            ensure!(
                state.publishers.get(publisher).is_some(),
                "publisher {publisher} is not configured in this image"
            );
            if let Some(installed) = state.installed.get(spec.name) {
                ensure!(
                    installed.published.manifest.publisher == publisher,
                    "installed package does not match publisher {publisher}: {}",
                    spec.name
                );
            }
        }
        let previous = selectors.insert(spec.name, spec.clone());
        ensure!(
            previous.is_none_or(|previous| previous == spec),
            "conflicting selectors for {}",
            spec.name
        );
    }
    for (name, spec) in selectors {
        if let Some(reason) = reason {
            let installed = state
                .installed
                .get(name)
                .map(|package| &package.published.manifest.version);
            let version = spec.version.as_ref().or(installed).with_context(|| {
                format!("{name} is not installed; specify the version to freeze")
            })?;
            ensure!(
                installed.is_none_or(|installed| installed == version),
                "installed package does not match the requested freeze version: {name}"
            );
            next.insert(
                name.to_owned(),
                FrozenPackage {
                    version: version.clone(),
                    reason: reason.clone(),
                },
            );
        } else {
            let freeze = next
                .get(name)
                .with_context(|| format!("package is not frozen: {name}"))?;
            ensure!(
                spec.version
                    .as_ref()
                    .is_none_or(|version| version == &freeze.version),
                "frozen package does not match the requested version: {name}"
            );
            next.remove(name);
        }
    }
    let changes = state
        .settings
        .freezes
        .keys()
        .chain(next.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter_map(|name| {
            let from = state.settings.freezes.get(name);
            let to = next.get(name);
            (from != to).then(|| FreezeChange {
                name: name.clone(),
                from: from.cloned(),
                to: to.cloned(),
            })
        })
        .collect();
    Ok((next, changes))
}
