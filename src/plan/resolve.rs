use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};

use crate::{
    image::{Installed, State},
    model::DependencyKind,
    repo::Published,
};

pub fn resolve(
    state: &State,
    packages: &[Published],
    requested: &BTreeSet<String>,
    update: bool,
) -> Result<BTreeMap<String, Installed>> {
    let mut candidates = BTreeMap::<_, Vec<_>>::new();
    for package in packages
        .iter()
        .chain(state.installed.values().map(|pkg| &pkg.published))
    {
        let manifest = &package.manifest;
        let previous = state.installed.get(&manifest.name);
        if let Some(previous) = previous {
            let old = &previous.published.manifest;
            if old.publisher != manifest.publisher || old.version > manifest.version {
                continue;
            }
            ensure!(
                old.version != manifest.version || package.sha256 == previous.published.sha256,
                "conflicting digests for {}",
                manifest.id()
            );
        }
        candidates
            .entry(manifest.name.as_str())
            .or_default()
            .push(package);
    }
    for (name, versions) in &mut candidates {
        let prefer_current = !update && !requested.contains(*name);
        let current = state.installed.get(*name).map(|pkg| &pkg.published.sha256);
        versions.sort_by_key(|pkg| {
            (
                prefer_current && current == Some(&pkg.sha256),
                pkg.manifest.version,
            )
        });
        versions.reverse();
        versions.dedup_by(|a, b| a.sha256 == b.sha256);
    }

    let wanted = state
        .installed
        .keys()
        .chain(requested)
        .map(String::as_str)
        .collect();
    let mut selected = BTreeMap::new();
    search(&candidates, wanted, &mut selected).context("cannot satisfy package constraints")?;
    Ok(selected
        .into_iter()
        .map(|(name, pkg)| {
            let explicit = requested.contains(name)
                || state.installed.get(name).is_some_and(|old| old.explicit);
            (
                name.to_owned(),
                Installed {
                    published: pkg.clone(),
                    explicit,
                },
            )
        })
        .collect())
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
        selected.insert(name, package);
        let mut wanted = wanted.clone();
        for dependency in &package.manifest.dependency {
            if dependency.kind == DependencyKind::Require
                && !selected.contains_key(dependency.package.as_str())
            {
                wanted.insert(dependency.package.as_str());
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
            if let Some(target) = selected.get(dependency.package.as_str()) {
                ensure!(
                    dependency.matches(target.manifest.version)?,
                    "{} requires {} {} when present, but selected {}",
                    package.manifest.id(),
                    dependency.package,
                    dependency.version.as_deref().unwrap_or("at any version"),
                    target.manifest.id()
                );
            }
        }
    }
    Ok(())
}

pub fn check(installed: &BTreeMap<String, Installed>) -> Result<()> {
    let selected: BTreeMap<_, _> = installed
        .iter()
        .map(|(name, pkg)| (name.as_str(), &pkg.published))
        .collect();
    check_selected(&selected)?;
    for package in selected.values() {
        for dependency in &package.manifest.dependency {
            if dependency.kind == DependencyKind::Require
                && !selected.contains_key(dependency.package.as_str())
            {
                bail!(
                    "{} is missing required package {}",
                    package.manifest.id(),
                    dependency.package
                );
            }
        }
    }
    Ok(())
}
