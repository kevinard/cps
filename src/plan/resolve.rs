mod sat;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};

use super::SolverRevision;
use crate::{
    image::Installed,
    model::{Dependency, DependencyTarget, PackageSpec},
    repo::Published,
};

pub fn resolve<T>(
    installed: &BTreeMap<String, Installed>,
    packages: &[Published],
    requested: &BTreeMap<&str, PackageSpec<'_>>,
    update: bool,
    exact: bool,
    solver: Option<SolverRevision>,
    compose: impl Fn(&BTreeMap<String, Installed>) -> Result<T>,
) -> Result<(BTreeMap<String, Installed>, T)> {
    let mut candidates = BTreeMap::<_, Vec<_>>::new();
    for package in packages
        .iter()
        .chain(installed.values().map(|pkg| &pkg.published))
    {
        let manifest = &package.manifest;
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
        if let Some(version) = spec.version {
            ensure!(
                candidates.contains_key(spec.name),
                "no eligible version for {}@{version}",
                spec.name
            );
        }
    }

    let wanted: BTreeSet<_> = installed
        .keys()
        .filter(|_| !exact)
        .map(String::as_str)
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
                    || (!exact && installed.get(name).is_some_and(|old| old.explicit));
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
        let composed = compose(&installed)?;
        Ok((installed, composed))
    };
    match solver {
        Some(SolverRevision::SatV1) => {
            let retained = installed
                .iter()
                .filter(|(name, _)| wanted.contains(name.as_str()))
                .map(|(name, entry)| (name.as_str(), entry.published.manifest.version))
                .collect();
            sat::solve(&candidates, &wanted, &retained, materialize)
        }
        None => {
            // Existing saved plans and pinned build recipes retain their original selection order.
            let mut selected = BTreeMap::new();
            search(&candidates, wanted.clone(), &mut selected)
                .context("cannot satisfy package constraints")?;
            materialize(selected)
        }
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
                Dependency::RequireAny { .. } | Dependency::Conditional { .. } => continue,
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
    for package in selected.values() {
        for dependency in &package.manifest.dependency {
            match dependency {
                Dependency::Require(target) => ensure!(
                    selected.contains_key(target.package.as_str()),
                    "{} is missing required package {}",
                    package.manifest.id(),
                    target.package,
                ),
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
