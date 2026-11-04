//! Read-only queries of committed image state and the configured signed catalog.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
};

use anyhow::{Context, Result, ensure};

use crate::{
    image::{Image, Installed, Publisher, Publishers},
    kdl_format::manifest_text,
    model::{PackageSpec, Version, validate_digest},
    repo::{CatalogPin, catalog_index, read_package},
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

struct CatalogEntry {
    id: String,
    name: String,
    version: Version,
    sha256: String,
}

fn available(
    publisher: &Publisher,
    pin: Option<&CatalogPin>,
) -> Result<(Vec<CatalogEntry>, BTreeMap<String, Version>)> {
    let index = catalog_index(
        &publisher.repo,
        &publisher.name,
        &publisher.key,
        pin.map(|pin| pin.sha256.as_str()),
    )?;
    let mut entries = Vec::new();
    for (id, sha256) in index.entries {
        let selector =
            PackageSpec::parse(&id).with_context(|| format!("invalid catalog ID {id}"))?;
        ensure!(
            selector.publisher == Some(publisher.name.as_str()),
            "catalog entry {id} has a different publisher"
        );
        let version = selector
            .version
            .with_context(|| format!("catalog entry {id} has no version"))?;
        validate_digest(&sha256)?;
        entries.push(CatalogEntry {
            name: selector.name.to_owned(),
            id,
            version,
            sha256,
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
    let pins = publishers.release_pins(&settings.arch, &settings.facets, None)?;
    let mut entries = Vec::new();
    let mut blocked = BTreeSet::new();
    for publisher in publishers.as_slice() {
        let (mut catalog, obsoletions) = available(publisher, pins.get(&publisher.name))?;
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
                .filter(|package| package.published.sha256 == entry.sha256)
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
        let pins = publishers.release_pins(&settings.arch, &settings.facets, None)?;
        let source = selector.publisher.or_else(|| {
            installed
                .as_ref()
                .map(|pkg| pkg.published.manifest.publisher.as_str())
        });
        for publisher in publishers.as_slice() {
            if source.is_some_and(|name| publisher.name != name) {
                continue;
            }
            let (entries, obsoletions) = available(publisher, pins.get(&publisher.name))?;
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
                    read_package(&publisher.repo, &publisher.name, &entry.id, entry.sha256)?;
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
