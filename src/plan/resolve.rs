mod sat;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};

use super::freeze::{self, FrozenPackage};
use crate::{
    image::Installed,
    model::{Dependency, DependencyTarget, PackageSpec, Version, VersionConstraint},
    repo::{PackageInfo, Published},
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

pub(crate) type Package<'a> = &'a dyn PackageInfo;

pub fn resolve<'a, T, P: PackageInfo + 'a>(
    installed: &'a BTreeMap<String, Installed>,
    available: Candidates<'_, impl IntoIterator<Item = &'a P>>,
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
    let mut candidates = BTreeMap::<_, Vec<Package<'a>>>::new();
    let obsolete = |package: Package<'_>| {
        available
            .obsoletions
            .get(package.name())
            .is_some_and(|cutoff| package.version().semantic() <= cutoff.semantic())
    };
    let offered = available
        .packages
        .into_iter()
        .map(|package| package as Package<'a>);
    for package in offered.chain(installed.values().map(|pkg| &pkg.published as Package<'a>)) {
        if obsolete(package) {
            continue;
        }
        let (name, version) = (package.name(), package.version());
        if available
            .freezes
            .get(name)
            .is_some_and(|freeze| &freeze.version != version)
        {
            continue;
        }
        let spec = requested.get(name);
        let pinned = spec.and_then(|spec| spec.version.as_ref());
        let previous = installed.get(name);
        if let Some(previous) = previous {
            let old = &previous.published.manifest;
            if old.publisher != package.publisher()
                || (old.version.semantic() > version.semantic() && pinned != Some(version))
            {
                continue;
            }
            ensure!(
                &old.version != version || package.sha256() == previous.published.sha256,
                "conflicting digests for {}",
                package.id()
            );
        }
        if spec.is_some_and(|spec| {
            spec.publisher
                .is_some_and(|publisher| publisher != package.publisher())
                || pinned.is_some_and(|pinned| pinned != version)
        }) {
            continue;
        }
        candidates.entry(name).or_default().push(package);
    }
    for (name, versions) in &mut candidates {
        let prefer_current = !update && !requested.contains_key(*name);
        let current = installed
            .get(*name)
            .map(|pkg| pkg.published.sha256.as_str());
        // Prefer the newest release. A respelling of the installed release is
        // not an upgrade, so the installed package wins a semantic tie.
        versions.sort_by(|a, b| {
            let (a_current, b_current) = (current == Some(a.sha256()), current == Some(b.sha256()));
            (prefer_current && a_current)
                .cmp(&(prefer_current && b_current))
                .then_with(|| a.version().semantic().cmp(&b.version().semantic()))
                .then_with(|| a_current.cmp(&b_current))
                .then_with(|| a.version().cmp(b.version()))
        });
        versions.reverse();
        versions.dedup_by(|a, b| a.sha256() == b.sha256());
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
    // Only names reachable from the request can be selected, so the solver
    // never sees the rest of a large catalog.
    let candidates = reachable(candidates, &wanted);
    let materialize = |selected: BTreeMap<&str, Package<'a>>| {
        for name in &wanted {
            ensure!(
                selected.contains_key(name),
                "resolver omitted required package {name}"
            );
        }
        // Full manifests load only for the selection, once per candidate.
        let installed = selected
            .into_iter()
            .map(|(name, pkg)| {
                let explicit = requested.contains_key(name)
                    || (retention.contains(name)
                        && installed.get(name).is_some_and(|old| old.explicit));
                Ok((
                    name.to_owned(),
                    Installed {
                        published: pkg.load()?,
                        explicit,
                    },
                ))
            })
            .collect::<Result<_>>()?;
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

/// Keep the candidates of names that a requirement chain from `wanted` can
/// reach: requirements, alternatives, conditional targets, capability
/// providers, and recommendations. Constraints never add a package.
fn reachable<'n, 'a>(
    mut candidates: BTreeMap<&'n str, Vec<Package<'a>>>,
    wanted: &BTreeSet<&str>,
) -> BTreeMap<&'n str, Vec<Package<'a>>> {
    let mut providers = BTreeMap::<&str, BTreeSet<&str>>::new();
    for (&name, versions) in &candidates {
        for package in versions {
            for capability in package.provides().keys() {
                providers.entry(capability).or_default().insert(name);
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut pending: Vec<&str> = wanted.iter().copied().collect();
    while let Some(name) = pending.pop() {
        if !seen.insert(name) {
            continue;
        }
        let Some(versions) = candidates.get(name) else {
            continue;
        };
        for package in versions {
            for dependency in package.dependencies() {
                match dependency {
                    Dependency::Require(target) | Dependency::Recommend(target) => {
                        pending.push(&target.package);
                    }
                    Dependency::Conditional { target, .. } => pending.push(&target.package),
                    Dependency::RequireAny { alternatives } => {
                        pending.extend(alternatives.iter().map(|target| target.package.as_str()));
                    }
                    Dependency::RequireCapability(target) => pending.extend(
                        providers
                            .get(target.package.as_str())
                            .into_iter()
                            .flatten()
                            .copied(),
                    ),
                    Dependency::Incorporate(_) | Dependency::Exclude(_) => {}
                }
            }
        }
    }
    let seen: BTreeSet<String> = seen.into_iter().map(str::to_owned).collect();
    candidates.retain(|name, _| seen.contains(*name));
    candidates
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
