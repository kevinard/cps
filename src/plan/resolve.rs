mod sat;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};

use super::SolverRevision;
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
    solver: Option<SolverRevision>,
    compose: impl Fn(&BTreeMap<String, Installed>) -> Result<T>,
) -> Result<(BTreeMap<String, Installed>, T)> {
    for (name, spec) in requested {
        if let Some(frozen) = available.freezes.get(*name) {
            ensure!(
                spec.version.is_none_or(|version| version == frozen.version),
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
            .is_some_and(|cutoff| manifest.version <= *cutoff)
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
        let pinned = spec.and_then(|spec| spec.version);
        let previous = installed.get(&manifest.name);
        if let Some(previous) = previous {
            let old = &previous.published.manifest;
            if old.publisher != manifest.publisher
                || (old.version > manifest.version && pinned != Some(manifest.version))
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
                || pinned.is_some_and(|version| version != manifest.version)
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
        versions.sort_by_key(|pkg| {
            (
                prefer_current && current == Some(&pkg.sha256),
                pkg.manifest.version,
            )
        });
        versions.reverse();
        versions.dedup_by(|a, b| a.sha256 == b.sha256);
    }
    for spec in requested.values() {
        if let Some(cutoff) = available.obsoletions.get(spec.name) {
            ensure!(
                spec.version.map_or_else(
                    || candidates.contains_key(spec.name),
                    |version| version > *cutoff
                ),
                "{} is obsolete through {cutoff}",
                spec.name
            );
        }
        if let Some(version) = spec.version {
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
    let result = match solver {
        Some(SolverRevision::SatV1) => sat::solve(&candidates, &wanted, installed, materialize),
        None => {
            // Existing saved plans and pinned build recipes retain their original selection order.
            let mut selected = BTreeMap::new();
            search(&candidates, wanted.clone(), &mut selected)
                .context("cannot satisfy package constraints")
                .and_then(|()| materialize(selected))
        }
    };
    if available.freezes.is_empty() {
        result
    } else {
        result.with_context(|| {
            format!("resolving package selection with freezes: {}; use plan unfreeze to lift a constraint", available.freezes.iter().map(|(name, frozen)| format!("{name}@{}{}", frozen.version, freeze::reason_suffix(frozen))).collect::<Vec<_>>().join(", "))
        })
    }
}

fn search<'a>(
    candidates: &BTreeMap<&'a str, Vec<&'a Published>>,
    mut wanted: BTreeSet<&'a str>,
    selected: &mut BTreeMap<&'a str, &'a Published>,
) -> Result<()> {
    let Some(name) = wanted.pop_first() else {
        return Ok(());
    };
    if selected.contains_key(name) {
        return search(candidates, wanted, selected);
    }
    let versions = candidates
        .get(name)
        .with_context(|| format!("no candidate for {name}"))?;
    let mut failure = None;
    for package in versions {
        if let Some(action) =
            package
                .manifest
                .dependency
                .iter()
                .find_map(|dependency| match dependency {
                    Dependency::RequireCapability(_) => Some("require-capability"),
                    Dependency::RequireAny { .. } => Some("require-any"),
                    Dependency::Conditional { .. } => Some("conditional"),
                    _ => None,
                })
        {
            failure = Some(anyhow::anyhow!(
                "{} requires solver sat-v1 for {action}",
                package.manifest.id()
            ));
            continue;
        }
        selected.insert(name, package);
        let mut wanted = wanted.clone();
        for dependency in &package.manifest.dependency {
            if let Dependency::Require(target) = dependency
                && !selected.contains_key(target.package.as_str())
            {
                wanted.insert(target.package.as_str());
            }
        }
        let attempt = check_selected(selected).and_then(|()| search(candidates, wanted, selected));
        if attempt.is_ok() {
            return Ok(());
        }
        failure = attempt.err();
        selected.remove(name);
    }
    Err(failure.context("no compatible version")?).with_context(|| format!("selecting {name}"))
}

fn check_selected(selected: &BTreeMap<&str, &Published>) -> Result<()> {
    for package in selected.values() {
        for dependency in &package.manifest.dependency {
            let (target, exclude) = match dependency {
                Dependency::Require(target) | Dependency::Incorporate(target) => (target, false),
                Dependency::Exclude(target) => (target, true),
                Dependency::RequireCapability(_)
                | Dependency::RequireAny { .. }
                | Dependency::Conditional { .. } => continue,
            };
            if let Some(selected_target) = selected.get(target.package.as_str()) {
                let verb = if exclude { "excludes" } else { "requires" };
                ensure!(
                    target.matches(selected_target.manifest.version)? != exclude,
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
        Some(package) => target.matches(package.manifest.version),
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
        for (name, &version) in &package.manifest.provides {
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
                                .any(|&version| constraint.matches_provided(version))),
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
