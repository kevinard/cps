mod sat;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};

use super::freeze::{self, FrozenPackage};
use crate::{
    image::Installed,
    model::{Dependency, DependencyTarget, PackageSpec, Version, VersionConstraint},
    repo::Published,
};

pub(crate) struct Candidates<'a, P> {
    pub packages: P,
    pub obsoletions: &'a BTreeMap<String, Version>,
    pub freezes: &'a BTreeMap<String, FrozenPackage>,
    pub profile_packages: &'a BTreeMap<String, String>,
}

pub(crate) enum Retention<'a> {
    All,
    None,
    Names(&'a BTreeSet<String>),
}

impl Retention<'_> {
    fn contains(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Names(names) => names.contains(name),
        }
    }
}

pub fn resolve<'a, T>(
    installed: &'a BTreeMap<String, Installed>,
    available: Candidates<'_, impl IntoIterator<Item = &'a Published>>,
    requested: &BTreeMap<&str, PackageSpec<'_>>,
    update: bool,
    retention: Retention<'_>,
    install_recommends: bool,
    compose: impl Fn(&BTreeMap<String, Installed>) -> Result<T>,
) -> Result<(BTreeMap<String, Installed>, T)> {
    for (name, spec) in requested {
        if let Some(frozen) = available.freezes.get(*name) {
            ensure!(
                spec.version
                    .as_ref()
                    .is_none_or(|version| version == &frozen.version),
                "{name} is frozen at {}{}; use plan unfreeze before changing its version",
                frozen.version,
                freeze::reason_suffix(frozen)
            );
        }
    }
    let mut candidates = BTreeMap::<_, Vec<_>>::new();
    let obsolete = |package: &Published| {
        let manifest = &package.manifest;
        available
            .obsoletions
            .get(&manifest.name)
            .is_some_and(|cutoff| manifest.version.semantic() <= cutoff.semantic())
    };
    for package in available
        .packages
        .into_iter()
        .chain(installed.values().map(|pkg| &pkg.published))
    {
        if obsolete(package) {
            continue;
        }
        let manifest = &package.manifest;
        if available
            .freezes
            .get(&manifest.name)
            .is_some_and(|freeze| freeze.version != manifest.version)
        {
            continue;
        }
        let spec = requested.get(manifest.name.as_str());
        let pinned = spec.and_then(|spec| spec.version.as_ref());
        let previous = installed.get(&manifest.name);
        if let Some(previous) = previous {
            let old = &previous.published.manifest;
            if old.publisher != manifest.publisher
                || (old.version.semantic() > manifest.version.semantic()
                    && pinned != Some(&manifest.version))
            {
                continue;
            }
            ensure!(
                old.version != manifest.version || package.sha256 == previous.published.sha256,
                "conflicting digests for {}",
                manifest.id()
            );
        }
        if spec.is_some_and(|spec| {
            spec.publisher
                .is_some_and(|publisher| publisher != manifest.publisher)
                || pinned.is_some_and(|version| version != &manifest.version)
        }) {
            continue;
        }
        candidates
            .entry(manifest.name.as_str())
            .or_default()
            .push(package);
    }
    for (name, versions) in &mut candidates {
        let prefer_current = !update && !requested.contains_key(*name);
        let current = installed.get(*name).map(|pkg| &pkg.published.sha256);
        // Prefer the newest release. A respelling of the installed release is
        // not an upgrade, so the installed package wins a semantic tie.
        versions.sort_by_key(|pkg| {
            let is_current = current == Some(&pkg.sha256);
            (
                prefer_current && is_current,
                pkg.manifest.version.semantic(),
                is_current,
                &pkg.manifest.version,
            )
        });
        versions.reverse();
        versions.dedup_by(|a, b| a.sha256 == b.sha256);
    }
    for spec in requested.values() {
        if let Some(cutoff) = available.obsoletions.get(spec.name) {
            ensure!(
                spec.version.as_ref().map_or_else(
                    || candidates.contains_key(spec.name),
                    |version| version.semantic() > cutoff.semantic()
                ),
                "{} is obsolete through {cutoff}",
                spec.name
            );
        }
        if let Some(version) = &spec.version {
            ensure!(
                candidates.contains_key(spec.name),
                "no eligible version for {}@{version}",
                spec.name
            );
        }
    }

    let wanted: BTreeSet<_> = installed
        .iter()
        .filter(|(name, package)| {
            retention.contains(name)
                && (!obsolete(&package.published) || candidates.contains_key(name.as_str()))
        })
        .map(|(name, _)| name.as_str())
        .chain(requested.keys().copied())
        .collect();
    let materialize = |selected: BTreeMap<&str, &Published>| {
        for name in &wanted {
            ensure!(
                selected.contains_key(name),
                "resolver omitted required package {name}"
            );
        }
        let installed = selected
            .into_iter()
            .map(|(name, pkg)| {
                let explicit = requested.contains_key(name)
                    || (retention.contains(name)
                        && installed.get(name).is_some_and(|old| old.explicit));
                (
                    name.to_owned(),
                    Installed {
                        published: pkg.clone(),
                        explicit,
                    },
                )
            })
            .collect();
        check(&installed).context("validating the resolved package set")?;
        freeze::check(available.freezes, &installed)?;
        let composed = compose(&installed)?;
        Ok((installed, composed))
    };
    let result = sat::solve(
        &candidates,
        &wanted,
        installed,
        install_recommends.then_some(available.profile_packages),
        materialize,
    );
    if available.freezes.is_empty() {
        result
    } else {
        result.with_context(|| {
            format!("resolving package selection with freezes: {}; use plan unfreeze to lift a constraint", available.freezes.iter().map(|(name, frozen)| format!("{name}@{}{}", frozen.version, freeze::reason_suffix(frozen))).collect::<Vec<_>>().join(", "))
        })
    }
}

fn check_selected(selected: &BTreeMap<&str, &Published>) -> Result<()> {
    for package in selected.values() {
        for dependency in &package.manifest.dependency {
            let (target, exclude) = match dependency {
                Dependency::Require(target) | Dependency::Incorporate(target) => (target, false),
                Dependency::Exclude(target) => (target, true),
                Dependency::RequireCapability(_)
                | Dependency::Recommend(_)
                | Dependency::RequireAny { .. }
                | Dependency::Conditional { .. } => continue,
            };
            if let Some(selected_target) = selected.get(target.package.as_str()) {
                let verb = if exclude { "excludes" } else { "requires" };
                ensure!(
                    target.matches(&selected_target.manifest.version)? != exclude,
                    "{} {verb} {target}, but selected {}",
                    package.manifest.id(),
                    selected_target.manifest.id()
                );
            }
        }
    }
    Ok(())
}

fn selected_matches(
    selected: &BTreeMap<&str, &Published>,
    target: &DependencyTarget,
) -> Result<bool> {
    match selected.get(target.package.as_str()) {
        Some(package) => target.matches(&package.manifest.version),
        None => Ok(false),
    }
}

pub fn check(installed: &BTreeMap<String, Installed>) -> Result<()> {
    let selected: BTreeMap<_, _> = installed
        .iter()
        .map(|(name, pkg)| (name.as_str(), &pkg.published))
        .collect();
    check_selected(&selected)?;
    let mut capabilities = BTreeMap::<_, Vec<_>>::new();
    for package in selected.values() {
        for (name, version) in &package.manifest.provides {
            capabilities.entry(name.as_str()).or_default().push(version);
        }
    }
    for package in selected.values() {
        for dependency in &package.manifest.dependency {
            match dependency {
                Dependency::Require(target) => ensure!(
                    selected.contains_key(target.package.as_str()),
                    "{} is missing required package {}",
                    package.manifest.id(),
                    target.package,
                ),
                Dependency::RequireCapability(target) => {
                    let constraint = VersionConstraint::parse(target.version.as_deref())?;
                    ensure!(
                        capabilities
                            .get(target.package.as_str())
                            .is_some_and(|versions| versions
                                .iter()
                                .any(|version| constraint.matches_provided(version.as_ref()))),
                        "{} is missing a provider for capability {target}",
                        package.manifest.id(),
                    );
                }
                Dependency::RequireAny { alternatives } => {
                    let mut satisfied = false;
                    for target in alternatives {
                        if selected_matches(&selected, target)? {
                            satisfied = true;
                            break;
                        }
                    }
                    ensure!(
                        satisfied,
                        "{} requires one of: {}",
                        package.manifest.id(),
                        alternatives
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                Dependency::Conditional { target, predicate } => ensure!(
                    !selected_matches(&selected, predicate)?
                        || selected_matches(&selected, target)?,
                    "{} requires {target} when {predicate} is installed",
                    package.manifest.id(),
                ),
                _ => {}
            }
        }
    }
    Ok(())
}
