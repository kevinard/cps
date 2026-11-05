//! Read-only queries of committed image state and the configured signed catalog.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
};

use anyhow::{Context, Result, ensure};

use crate::{
    image::{Image, Installed, Publisher, Publishers, Settings},
    kdl_format::manifest_text,
    model::{PackageSpec, Version},
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
        let (catalog, obsolete) = available(
            &image,
            &publishers,
            &settings,
            publisher,
            pins.get(&publisher.name),
        )?;
        for (spec, sources) in choices.values_mut() {
            let matching = catalog.iter().find(|entry| {
                entry.name == spec.name
                    && spec.version.is_none_or(|version| version == entry.version)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    let image = Image::open_query(root)?;
    image.require_clean()?;
    let state = image.state()?;
    image.verify_catalog_archives(&state)?;
    let intent = image.profile_intent(&state)?;
    state
        .installed
        .iter()
        .map(|(name, package)| {
            package.verify(name, &state.publishers)?;
            let profile = if !state.settings.profile_packages.contains_key(name) {
                ProfileRole::None
            } else if intent.roots.contains(name) {
                ProfileRole::Root
            } else if intent.roots.is_empty() {
                ProfileRole::Protected
            } else {
                ProfileRole::Dependency
            };
            Ok(PackageIntentRow {
                id: package.published.manifest.id(),
                requested: intent.local.contains(name),
                profile,
            })
        })
        .collect()
}

/// Read explicitly configured facet values from the committed image.
pub fn list_facets(root: &Path) -> Result<BTreeMap<String, bool>> {
    let image = Image::open_query(root)?;
    image.require_clean()?;
    Ok(image.settings()?.facets)
}

struct AvailablePackage {
    id: String,
    name: String,
    version: Version,
    entry: CatalogEntry,
}

fn available(
    image: &Image,
    publishers: &Publishers,
    settings: &Settings,
    publisher: &Publisher,
    pin: Option<&CatalogPin>,
) -> Result<(Vec<AvailablePackage>, BTreeMap<String, Version>)> {
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
    Ok((entries, index.obsoletions))
}

fn installed(image: &Image, publishers: &Publishers) -> Result<BTreeMap<String, Installed>> {
    let installed = image.installed()?;
    for (name, package) in &installed {
        package.verify(name, publishers)?;
    }
    Ok(installed)
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
        return Ok(installed
            .into_iter()
            .map(|(name, package)| PackageRow {
                id: package.published.manifest.id(),
                name,
                status: status(&package),
            })
            .collect());
    }
    let settings = image.settings()?;
    let pins = image.release_pins(&publishers, &settings)?;
    let mut entries = Vec::new();
    let mut blocked = BTreeSet::new();
    for publisher in publishers.as_slice() {
        let (mut catalog, obsoletions) = available(
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
            }
        })
        .collect())
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
    let accepts = |version| selector.version.is_none_or(|wanted| wanted == version);
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
            let (entries, obsoletions) = available(
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
                        .map_or_else(|| named.peek().is_some(), |version| version > *cutoff),
                    "{} is obsolete through {cutoff} for {}",
                    selector.name,
                    publisher.name,
                );
            }
            if named.peek().is_some() || obsoletions.contains_key(selector.name) {
                let entry = named
                    .find(|entry| accepts(entry.version))
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
            .filter(|entry| {
                let manifest = &entry.published.manifest;
                accepts(manifest.version)
                    && selector
                        .publisher
                        .is_none_or(|name| name == manifest.publisher)
            })
            .with_context(|| format!("package is not installed: {spec}"))?
            .published
    };
    Ok(manifest_text(&package.manifest))
}
