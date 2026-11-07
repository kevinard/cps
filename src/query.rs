//! Read-only queries of committed image state and the configured signed catalog.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::{
    image::{Image, Installed, ProfileIntent, Publisher, Publishers, Settings, State},
    kdl_format::manifest_text,
    model::{Dependency, DependencyTarget, Manifest, PackageSpec, Version, validate_path},
    repo::{CatalogEntry, CatalogPin, catalog_index, read_package},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scope {
    Installed,
    Available,
    AllAvailableVersions,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Explicit,
    Dependency,
    Available,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Explicit => "explicit",
            Self::Dependency => "dependency",
            Self::Available => "available",
        })
    }
}

#[derive(Debug)]
pub struct PackageRow {
    pub id: String,
    pub name: String,
    pub status: Status,
    pub summary: Option<String>,
}

pub struct InstallSource {
    pub publisher: String,
    pub version: String,
}

pub(crate) type SelectSource<'a> = dyn FnMut(&str, &[InstallSource]) -> Result<usize> + 'a;

/// Qualify uninstalled package requests with their selected publisher.
pub(crate) fn select_install_sources(
    root: &Path,
    packages: &mut [String],
    choose: &mut SelectSource<'_>,
) -> Result<()> {
    let image = Image::open_query(root)?;
    image.require_clean()?;
    let publishers = image.publishers()?;
    if publishers.as_slice().len() < 2 {
        return Ok(());
    }
    let installed = installed(&image, &publishers)?;
    let specs = packages
        .iter()
        .map(|input| Ok((input.as_str(), PackageSpec::parse(input)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut choices = specs
        .into_iter()
        .filter(|(_, spec)| spec.publisher.is_none() && !installed.contains_key(spec.name))
        .map(|(input, spec)| (input.to_owned(), (spec, Vec::new())))
        .collect::<BTreeMap<_, _>>();
    if choices.is_empty() {
        return Ok(());
    }
    let settings = image.settings()?;
    let pins = image.release_pins(&publishers, &settings)?;
    for publisher in publishers.as_slice() {
        let AvailableCatalog {
            packages: catalog,
            obsoletions: obsolete,
            ..
        } = available(
            &image,
            &publishers,
            &settings,
            publisher,
            pins.get(&publisher.name),
        )?;
        for (spec, sources) in choices.values_mut() {
            let matching = catalog.iter().find(|entry| {
                entry.name == spec.name
                    && spec
                        .version
                        .as_ref()
                        .is_none_or(|version| version == &entry.version)
                    && obsolete
                        .get(spec.name)
                        .is_none_or(|cutoff| entry.version > *cutoff)
            });
            if let Some(entry) = matching {
                sources.push(InstallSource {
                    publisher: publisher.name.clone(),
                    version: entry.version.to_string(),
                });
            }
        }
    }
    drop(image);

    let mut selected = BTreeMap::new();
    for input in packages.iter() {
        let Some((spec, sources)) = choices.get(input).filter(|(_, sources)| sources.len() > 1)
        else {
            continue;
        };
        if selected.contains_key(input) {
            continue;
        }
        let index = choose(input, sources)?;
        let source = sources.get(index).context("invalid publisher selection")?;
        let version = spec
            .version
            .as_ref()
            .map(|version| format!("@{version}"))
            .unwrap_or_default();
        selected.insert(
            input.clone(),
            format!("cps://{}/{}{version}", source.publisher, spec.name),
        );
    }
    drop(choices);
    for package in packages {
        if let Some(qualified) = selected.get(package) {
            package.clone_from(qualified);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileRole {
    Root,
    Dependency,
    Protected,
    None,
}

impl fmt::Display for ProfileRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Root => "root",
            Self::Dependency => "dependency",
            Self::Protected => "protected",
            Self::None => "-",
        })
    }
}

#[derive(Debug)]
pub struct PackageIntentRow {
    pub id: String,
    pub requested: bool,
    pub profile: ProfileRole,
}

/// List effective local requests and roles in the applied profile, offline.
pub fn list_package_intent(root: &Path) -> Result<Vec<PackageIntentRow>> {
    let (state, intent) = read_intent(root)?;
    Ok(state
        .installed
        .values()
        .map(|package| intent_row(&state, &intent, package))
        .collect())
}

#[derive(Debug)]
pub struct PackageExplanation {
    pub intent: PackageIntentRow,
    pub profile_name: Option<String>,
    pub dependents: Vec<String>,
    pub recommenders: Vec<String>,
    pub path: Option<DependencyPath>,
}

#[derive(Debug)]
pub struct DependencyPath {
    pub root: PackageIntentRow,
    pub steps: Vec<DependencyStep>,
}

#[derive(Debug)]
pub struct DependencyStep {
    pub provider: String,
    pub requirement: String,
    pub matching_providers: usize,
}

/// Explain the current local request, signed profile role, and direct dependents.
pub fn explain_package(root: &Path, spec: &str) -> Result<PackageExplanation> {
    let selector = PackageSpec::parse(spec)?;
    let (state, intent) = read_intent(root)?;
    let package = select_installed(&state.installed, &selector, spec)?;
    let path = required_path(&state, &intent, selector.name)?;
    let intent = intent_row(&state, &intent, package);
    let profile_name = state
        .publishers
        .release_profile()
        .filter(|_| intent.profile != ProfileRole::None)
        .map(str::to_owned);
    let mut dependents = installed_dependents(&state.installed, selector.name)?;
    dependents.retain(|id| id != &intent.id);
    let mut recommenders = Vec::new();
    let version = &package.published.manifest.version;
    for (name, entry) in &state.installed {
        if name == selector.name {
            continue;
        }
        let manifest = &entry.published.manifest;
        for dependency in &manifest.dependency {
            if let Dependency::Recommend(target) = dependency
                && target.package == selector.name
                && target.matches(version)?
            {
                recommenders.push(manifest.id());
                break;
            }
        }
    }
    Ok(PackageExplanation {
        intent,
        profile_name,
        dependents,
        recommenders,
        path,
    })
}

fn required_path(
    state: &State,
    intent: &ProfileIntent,
    target: &str,
) -> Result<Option<DependencyPath>> {
    let is_root = |name: &str| intent.local.contains(name) || intent.roots.contains(name);
    if is_root(target) {
        return Ok(None);
    }
    let mut incoming = BTreeMap::<_, Vec<_>>::new();
    for package in state.installed.values() {
        let manifest = &package.published.manifest;
        for dependency in &manifest.dependency {
            let providers = matching_providers(dependency, &state.installed)?;
            for provider in &providers {
                incoming.entry(provider.name.as_str()).or_default().push((
                    manifest.name.as_str(),
                    dependency,
                    providers.len(),
                ));
            }
        }
    }
    let mut queue = VecDeque::from([target]);
    let mut seen = BTreeSet::from([target]);
    let mut next = BTreeMap::new();
    while let Some(provider) = queue.pop_front() {
        if is_root(provider) {
            let root = intent_row(state, intent, &state.installed[provider]);
            let mut steps = Vec::new();
            let mut parent = provider;
            while let Some(&(name, requirement, matching_providers)) = next.get(parent) {
                steps.push(DependencyStep {
                    provider: state.installed[name].published.manifest.id(),
                    requirement: describe_requirement(requirement),
                    matching_providers,
                });
                parent = name;
            }
            return Ok(Some(DependencyPath { root, steps }));
        }
        for &(parent, dependency, count) in incoming.get(provider).into_iter().flatten() {
            if seen.insert(parent) {
                next.insert(parent, (provider, dependency, count));
                queue.push_back(parent);
            }
        }
    }
    Ok(None)
}

fn describe_requirement(dependency: &Dependency) -> String {
    match dependency {
        Dependency::Require(target) => format!("requires {target}"),
        Dependency::RequireCapability(target) => format!("requires capability {target}"),
        Dependency::RequireAny { .. } => "requires one of several alternatives".into(),
        Dependency::Conditional { target, predicate } => {
            format!("requires {target} when {predicate} is installed")
        }
        _ => unreachable!("only active requirements have matching providers"),
    }
}

fn read_intent(root: &Path) -> Result<(State, ProfileIntent)> {
    verified_intent(&Image::open_query(root)?)
}

pub(crate) fn verified_intent(image: &Image) -> Result<(State, ProfileIntent)> {
    image.require_clean()?;
    let state = image.state()?;
    image.verify_catalog_archives(&state)?;
    let intent = image.profile_intent(&state)?;
    verify_installed(&state.installed, &state.publishers)?;
    Ok((state, intent))
}

pub(crate) fn intent_row(
    state: &State,
    intent: &ProfileIntent,
    package: &Installed,
) -> PackageIntentRow {
    let manifest = &package.published.manifest;
    let name = &manifest.name;
    let profile = if !state.settings.profile_packages.contains_key(name) {
        ProfileRole::None
    } else if intent.roots.contains(name) {
        ProfileRole::Root
    } else if intent.roots.is_empty() {
        ProfileRole::Protected
    } else {
        ProfileRole::Dependency
    };
    PackageIntentRow {
        id: manifest.id(),
        requested: intent.local.contains(name),
        profile,
    }
}

/// Read explicitly configured facet values from the committed image.
pub fn list_facets(root: &Path) -> Result<BTreeMap<String, bool>> {
    let image = Image::open_query(root)?;
    image.require_clean()?;
    Ok(image.settings()?.facets)
}

#[derive(Debug, Serialize)]
pub struct MediatorRow {
    pub group: String,
    pub preferred: Option<String>,
    pub selected: Option<String>,
    pub providers: BTreeMap<String, i32>,
}

pub fn list_mediators(root: &Path) -> Result<Vec<MediatorRow>> {
    let image = Image::open_query(root)?;
    let (state, _) = verified_intent(&image)?;
    let offers = crate::plan::mediator_offers(
        &state.settings.arch,
        &state.settings.facets,
        &state.installed,
    );
    let winners = crate::plan::choose_mediators(&offers, &state.settings.mediators);
    let groups: BTreeSet<_> = offers
        .keys()
        .copied()
        .chain(state.settings.mediators.keys().map(String::as_str))
        .collect();
    Ok(groups
        .into_iter()
        .map(|group| MediatorRow {
            group: group.to_owned(),
            preferred: state.settings.mediators.get(group).cloned(),
            selected: winners.get(group).map(|winner| (*winner).to_owned()),
            providers: offers
                .get(group)
                .into_iter()
                .flatten()
                .map(|(&name, &priority)| (name.to_owned(), priority))
                .collect(),
        })
        .collect())
}

struct AvailablePackage {
    id: String,
    name: String,
    version: Version,
    entry: CatalogEntry,
}

struct AvailableCatalog {
    packages: Vec<AvailablePackage>,
    obsoletions: BTreeMap<String, Version>,
    file_index: Option<String>,
    catalog: String,
}

fn available(
    image: &Image,
    publishers: &Publishers,
    settings: &Settings,
    publisher: &Publisher,
    pin: Option<&CatalogPin>,
) -> Result<AvailableCatalog> {
    let index = catalog_index(&publisher.repo, &publisher.name, &publisher.key, pin)?;
    image.check_catalog(publishers, settings, publisher, &index.checkpoint)?;
    let mut entries = Vec::new();
    for (id, entry) in index.entries {
        if !entry.supports_architecture(&settings.arch) {
            continue;
        }
        let selector =
            PackageSpec::parse(&id).with_context(|| format!("invalid catalog ID {id}"))?;
        ensure!(
            selector.publisher == Some(publisher.name.as_str()),
            "catalog entry {id} has a different publisher"
        );
        let version = selector
            .version
            .with_context(|| format!("catalog entry {id} has no version"))?;
        entries.push(AvailablePackage {
            name: selector.name.to_owned(),
            id,
            version,
            entry,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| b.version.cmp(&a.version)));
    Ok(AvailableCatalog {
        packages: entries,
        obsoletions: index.obsoletions,
        file_index: index.file_index,
        catalog: index.checkpoint.catalog,
    })
}

#[derive(Debug, Serialize)]
pub struct PackagePolicy {
    pub package: String,
    pub installed: Option<String>,
    pub preferred_publisher: Option<String>,
    pub freeze: Option<crate::FrozenPackage>,
    pub profile: Option<String>,
    pub applied_manifest: Option<String>,
    pub target_manifest: Option<String>,
    pub publishers: Vec<PolicyPublisher>,
    pub install_probe: Option<InstallProbe>,
}

#[derive(Debug, Serialize)]
pub struct PolicyPublisher {
    pub name: String,
    pub catalog: String,
    pub obsoleted_through: Option<Version>,
    pub requires_build_trust: bool,
    pub candidates: Vec<PolicyCandidate>,
}

#[derive(Debug, Serialize)]
pub struct PolicyCandidate {
    pub id: String,
    pub manifest: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InstallProbe {
    Solved { plan: String },
    Blocked { reason: String },
}

/// Inspect candidate sources and image policy without refreshing catalogs.
pub fn package_policy(root: &Path, spec: &str, probe: bool) -> Result<PackagePolicy> {
    let selector = PackageSpec::parse(spec)?;
    let image = Image::open_query(root)?;
    let (state, _) = verified_intent(&image)?;
    let profile = image.select_profile(&state.publishers, &state.settings, None)?;
    let installed = state.installed.get(selector.name);
    let mut preferred = installed
        .map(|package| package.published.manifest.publisher.clone())
        .or_else(|| selector.publisher.map(str::to_owned));
    let mut publishers = Vec::new();
    for publisher in state.publishers.as_slice() {
        let pin = profile
            .as_ref()
            .and_then(|profile| profile.pin(&publisher.name));
        let available = available(&image, &state.publishers, &state.settings, publisher, pin)?;
        let obsoleted_through = available.obsoletions.get(selector.name).cloned();
        let candidates: Vec<_> = available
            .packages
            .into_iter()
            .filter(|entry| entry.name == selector.name)
            .map(|entry| PolicyCandidate {
                id: entry.id,
                manifest: entry.entry.manifest_digest().to_owned(),
            })
            .collect();
        if preferred.is_none() && (!candidates.is_empty() || obsoleted_through.is_some()) {
            preferred = Some(publisher.name.clone());
        }
        publishers.push(PolicyPublisher {
            name: publisher.name.clone(),
            catalog: available.catalog,
            candidates,
            obsoleted_through,
            requires_build_trust: publisher.build_trust.is_some()
                || pin.is_some_and(|pin| pin.build_trust.is_some()),
        });
    }
    let install_probe = probe.then(|| {
        match crate::plan(
            root,
            crate::Request::Install {
                packages: vec![spec.to_owned()],
            },
        ) {
            Ok(plan) => InstallProbe::Solved {
                plan: plan.summary(),
            },
            Err(error) => InstallProbe::Blocked {
                reason: format!("{error:#}"),
            },
        }
    });
    Ok(PackagePolicy {
        package: spec.to_owned(),
        installed: installed.map(|package| package.published.manifest.id()),
        preferred_publisher: preferred,
        freeze: state.settings.freezes.get(selector.name).cloned(),
        profile: state.publishers.release_profile().map(str::to_owned),
        applied_manifest: state.settings.profile_packages.get(selector.name).cloned(),
        target_manifest: profile
            .and_then(|profile| profile.record.composition.get(selector.name).cloned()),
        publishers,
        install_probe,
    })
}

fn installed(image: &Image, publishers: &Publishers) -> Result<BTreeMap<String, Installed>> {
    let installed = image.installed()?;
    verify_installed(&installed, publishers)?;
    Ok(installed)
}

fn verify_installed(
    installed: &BTreeMap<String, Installed>,
    publishers: &Publishers,
) -> Result<()> {
    for (name, package) in installed {
        package.verify(name, publishers)?;
    }
    Ok(())
}

fn status(package: &Installed) -> Status {
    if package.explicit {
        Status::Explicit
    } else {
        Status::Dependency
    }
}

/// List packages without modifying image state or fetching from a mirror.
pub fn list_packages(root: &Path, scope: Scope) -> Result<Vec<PackageRow>> {
    let image = Image::open_query(root)?;
    image.require_clean()?;
    let publishers = image.publishers()?;
    let installed = installed(&image, &publishers)?;
    if scope == Scope::Installed {
        if installed
            .values()
            .any(|pkg| pkg.published.manifest.summary.is_some())
        {
            verify_installed_metadata(&image)?;
        }
        return Ok(installed
            .into_iter()
            .map(|(name, package)| PackageRow {
                id: package.published.manifest.id(),
                name,
                status: status(&package),
                summary: package.published.manifest.summary,
            })
            .collect());
    }
    let settings = image.settings()?;
    let pins = image.release_pins(&publishers, &settings)?;
    let mut entries = Vec::new();
    let mut blocked = BTreeSet::new();
    for publisher in publishers.as_slice() {
        let AvailableCatalog {
            packages: mut catalog,
            obsoletions,
            ..
        } = available(
            &image,
            &publishers,
            &settings,
            publisher,
            pins.get(&publisher.name),
        )?;
        if scope == Scope::Available {
            let from_source = |name: &str| {
                installed
                    .get(name)
                    .is_none_or(|pkg| pkg.published.manifest.publisher == publisher.name)
            };
            catalog.retain(|entry| from_source(&entry.name) && !blocked.contains(&entry.name));
            blocked.extend(obsoletions.into_keys().filter(|name| from_source(name)));
        }
        entries.extend(catalog);
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    if scope == Scope::Available {
        entries.dedup_by(|a, b| a.name == b.name);
    }
    Ok(entries
        .into_iter()
        .map(|entry| {
            let status = installed
                .get(&entry.name)
                .filter(|package| package.published.sha256 == entry.entry.manifest_digest())
                .map_or(Status::Available, status);
            PackageRow {
                id: entry.id,
                name: entry.name,
                status,
                summary: entry.entry.summary().map(str::to_owned),
            }
        })
        .collect())
}

fn verify_installed_metadata(image: &Image) -> Result<()> {
    let state = image.state()?;
    ensure!(
        state.settings.catalog_archives,
        "missing signed catalog archives for installed package metadata"
    );
    image.verify_catalog_archives(&state)
}

#[derive(Debug, Eq, PartialEq)]
pub struct FileMatch {
    pub package: String,
    pub path: String,
}

/// Search the latest compatible package from each publisher by exact path or basename.
pub fn search_files(root: &Path, term: &str) -> Result<Vec<FileMatch>> {
    let by_path = term.contains('/');
    let term = term.trim_start_matches('/');
    validate_path(term)?;

    let image = Image::open_query(root)?;
    image.require_clean()?;
    let publishers = image.publishers()?;
    let settings = image.settings()?;
    let pins = image.release_pins(&publishers, &settings)?;
    let mut matches = Vec::new();
    for publisher in publishers.as_slice() {
        let AvailableCatalog {
            packages: mut entries,
            file_index,
            ..
        } = available(
            &image,
            &publishers,
            &settings,
            publisher,
            pins.get(&publisher.name),
        )?;
        entries.dedup_by(|a, b| a.name == b.name);
        if let Some(index) = file_index {
            let index = crate::repo::read_file_index(&publisher.repo, &index)?;
            let basename = term.rsplit('/').next().unwrap_or(term);
            let Some(candidates) = index.get(basename) else {
                continue;
            };
            entries.retain(|entry| candidates.contains(&entry.id));
        }
        for entry in entries {
            let package = read_package(&publisher.repo, &publisher.name, &entry.id, &entry.entry)
                .with_context(|| format!("reading manifest for {}", entry.id))?;
            let paths = package
                .manifest
                .action
                .iter()
                .filter(|action| action.applies_to(&settings.arch, &settings.facets))
                .map(|action| action.path())
                .filter(|path| {
                    if by_path {
                        *path == term
                    } else {
                        path.rsplit('/').next() == Some(term)
                    }
                })
                .collect::<BTreeSet<_>>();
            matches.extend(paths.into_iter().map(|path| FileMatch {
                package: entry.id.clone(),
                path: path.to_owned(),
            }));
        }
    }
    Ok(matches)
}

/// Return the installed package IDs that own an exact image path.
pub fn path_owners(root: &Path, path: &str) -> Result<Vec<String>> {
    let relative = path.strip_prefix('/').unwrap_or(path);
    validate_path(relative)?;
    let image = Image::open_query(root)?;
    image.require_clean()?;
    let managed = image
        .managed_path(relative)?
        .filter(|entry| !entry.owners.is_empty())
        .with_context(|| format!("no installed package owns {path:?}"))?;
    let publishers = image.publishers()?;
    managed
        .owners
        .into_iter()
        .map(|name| {
            let package = image
                .installed_package(&name)?
                .with_context(|| format!("path {path:?} has an uninstalled owner {name}"))?;
            package.verify(&name, &publishers)?;
            Ok(package.published.manifest.id())
        })
        .collect()
}

/// List the root-relative paths owned by an installed package.
pub fn package_contents(root: &Path, spec: &str) -> Result<Vec<String>> {
    let selector = PackageSpec::parse(spec)?;
    let image = Image::open_query(root)?;
    image.require_clean()?;
    let package = image
        .installed_package(selector.name)?
        .filter(|package| matches_installed(package, &selector))
        .with_context(|| format!("package is not installed: {spec}"))?;
    package.verify(selector.name, &image.publishers()?)?;
    let paths = package
        .published
        .manifest
        .action
        .iter()
        .map(|action| action.path())
        .collect::<BTreeSet<_>>();
    let mut owned = Vec::new();
    for path in paths {
        if image
            .managed_path(path)?
            .is_some_and(|entry| entry.owners.contains(selector.name))
        {
            owned.push(path.to_owned());
        }
    }
    Ok(owned)
}

/// List installed package IDs that satisfy this package's direct requirements.
pub fn direct_dependencies(root: &Path, spec: &str) -> Result<Vec<String>> {
    let (installed, name) = dependency_query(root, spec)?;
    let mut dependencies = BTreeMap::new();
    for dependency in &installed[name].published.manifest.dependency {
        for manifest in matching_providers(dependency, &installed)? {
            dependencies.insert(&manifest.name, manifest);
        }
    }
    Ok(dependencies
        .values()
        .map(|manifest| manifest.id())
        .collect())
}

fn matching_providers<'a>(
    dependency: &Dependency,
    installed: &'a BTreeMap<String, Installed>,
) -> Result<Vec<&'a Manifest>> {
    let mut providers = BTreeMap::new();
    if let Dependency::RequireCapability(target) = dependency {
        for package in installed.values() {
            let manifest = &package.published.manifest;
            if target.matches_capability(manifest)? {
                providers.insert(&manifest.name, manifest);
            }
        }
    } else {
        for target in active_targets(dependency, installed)? {
            if let Some(package) = installed.get(&target.package)
                && target.matches(&package.published.manifest.version)?
            {
                let manifest = &package.published.manifest;
                providers.insert(&manifest.name, manifest);
            }
        }
    }
    Ok(providers.into_values().collect())
}

/// List installed package IDs with a direct requirement satisfied by this package.
pub fn reverse_dependencies(root: &Path, spec: &str) -> Result<Vec<String>> {
    let (installed, name) = dependency_query(root, spec)?;
    installed_dependents(&installed, name)
}

fn installed_dependents(
    installed: &BTreeMap<String, Installed>,
    name: &str,
) -> Result<Vec<String>> {
    let provider = &installed[name].published.manifest;
    let mut dependents = Vec::new();
    'packages: for package in installed.values() {
        let manifest = &package.published.manifest;
        for dependency in &manifest.dependency {
            if let Dependency::RequireCapability(target) = dependency
                && target.matches_capability(provider)?
            {
                dependents.push(manifest.id());
                continue 'packages;
            }
            for target in active_targets(dependency, installed)? {
                if target.package == name && target.matches(&provider.version)? {
                    dependents.push(manifest.id());
                    continue 'packages;
                }
            }
        }
    }
    Ok(dependents)
}

fn dependency_query<'a>(
    root: &Path,
    spec: &'a str,
) -> Result<(BTreeMap<String, Installed>, &'a str)> {
    let selector = PackageSpec::parse(spec)?;
    let image = Image::open_query(root)?;
    image.require_clean()?;
    let installed = installed(&image, &image.publishers()?)?;
    select_installed(&installed, &selector, spec)?;
    Ok((installed, selector.name))
}

fn select_installed<'a>(
    installed: &'a BTreeMap<String, Installed>,
    selector: &PackageSpec<'_>,
    spec: &str,
) -> Result<&'a Installed> {
    installed
        .get(selector.name)
        .filter(|package| matches_installed(package, selector))
        .with_context(|| format!("package is not installed: {spec}"))
}

fn active_targets<'a>(
    dependency: &'a Dependency,
    installed: &BTreeMap<String, Installed>,
) -> Result<&'a [DependencyTarget]> {
    Ok(match dependency {
        Dependency::Require(target) => std::slice::from_ref(target),
        Dependency::RequireAny { alternatives } => alternatives,
        Dependency::Conditional { target, predicate } => match installed.get(&predicate.package) {
            Some(package) if predicate.matches(&package.published.manifest.version)? => {
                std::slice::from_ref(target)
            }
            _ => &[],
        },
        Dependency::RequireCapability(_)
        | Dependency::Recommend(_)
        | Dependency::Incorporate(_)
        | Dependency::Exclude(_) => &[],
    })
}

fn matches_installed(package: &Installed, selector: &PackageSpec<'_>) -> bool {
    let manifest = &package.published.manifest;
    selector
        .version
        .as_ref()
        .is_none_or(|version| version == &manifest.version)
        && selector
            .publisher
            .is_none_or(|publisher| publisher == manifest.publisher)
}

/// Return the verified manifest for a package in KDL.
pub fn package_manifest(root: &Path, spec: &str, from_catalog: bool) -> Result<String> {
    let selector = PackageSpec::parse(spec)?;
    let image = Image::open_query(root)?;
    image.require_clean()?;
    let publishers = image.publishers()?;
    if let Some(name) = selector.publisher {
        ensure!(
            publishers.get(name).is_some(),
            "publisher {name} is not configured in this image"
        );
    }
    let installed = image.installed_package(selector.name)?;
    if let Some(package) = &installed {
        package.verify(selector.name, &publishers)?;
    }
    let accepts = |version: &Version| {
        selector
            .version
            .as_ref()
            .is_none_or(|wanted| wanted == version)
    };
    let package = if from_catalog {
        let settings = image.settings()?;
        let pins = image.release_pins(&publishers, &settings)?;
        let source = selector.publisher.or_else(|| {
            installed
                .as_ref()
                .map(|pkg| pkg.published.manifest.publisher.as_str())
        });
        for publisher in publishers.as_slice() {
            if source.is_some_and(|name| publisher.name != name) {
                continue;
            }
            let AvailableCatalog {
                packages: entries,
                obsoletions,
                ..
            } = available(
                &image,
                &publishers,
                &settings,
                publisher,
                pins.get(&publisher.name),
            )?;
            let mut named = entries
                .into_iter()
                .filter(|entry| entry.name == selector.name)
                .peekable();
            if let Some(cutoff) = obsoletions.get(selector.name) {
                ensure!(
                    selector
                        .version
                        .as_ref()
                        .map_or_else(|| named.peek().is_some(), |version| version > cutoff),
                    "{} is obsolete through {cutoff} for {}",
                    selector.name,
                    publisher.name,
                );
            }
            if named.peek().is_some() || obsoletions.contains_key(selector.name) {
                let entry = named
                    .find(|entry| accepts(&entry.version))
                    .with_context(|| {
                        format!(
                            "package is absent from the signed catalog for {}: {spec}",
                            publisher.name
                        )
                    })?;
                let package =
                    read_package(&publisher.repo, &publisher.name, &entry.id, &entry.entry)?;
                return Ok(manifest_text(&package.manifest));
            }
        }
        anyhow::bail!("package is absent from the signed catalog: {spec}")
    } else {
        installed
            .filter(|package| matches_installed(package, &selector))
            .with_context(|| format!("package is not installed: {spec}"))?
            .published
    };
    if package.manifest.summary.is_some() || !package.manifest.optional_dependencies.is_empty() {
        verify_installed_metadata(&image)?;
    }
    Ok(manifest_text(&package.manifest))
}
