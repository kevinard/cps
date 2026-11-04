//! Read-only queries of committed image state and the configured signed catalog.

use std::{collections::BTreeMap, fmt, path::Path};

use anyhow::{Context, Result, ensure};

use crate::{
    image::{Image, Installed, Publisher},
    kdl_format::manifest_text,
    model::{Version, validate_digest, validate_name, validate_publisher},
    repo::{catalog_entries, read_package},
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

struct Selector<'a> {
    publisher: Option<&'a str>,
    name: &'a str,
    version: Option<Version>,
}

impl<'a> Selector<'a> {
    fn parse(text: &'a str) -> Result<Self> {
        let (publisher, spec) = if let Some(spec) = text.strip_prefix("cps://") {
            let (publisher, spec) = spec.split_once('/').context("package ID has no name")?;
            validate_publisher(publisher)?;
            (Some(publisher), spec)
        } else {
            (None, text)
        };
        let (name, version) = if let Some((name, version)) = spec.split_once('@') {
            (name, Some(version.parse()?))
        } else {
            (spec, None)
        };
        validate_name(name)?;
        Ok(Self {
            publisher,
            name,
            version,
        })
    }
}

struct CatalogEntry {
    id: String,
    name: String,
    version: Version,
    sha256: String,
}

fn available(publisher: &Publisher) -> Result<Vec<CatalogEntry>> {
    let mut entries = Vec::new();
    for (id, sha256) in catalog_entries(&publisher.repo, &publisher.name, &publisher.key)? {
        let selector = Selector::parse(&id).with_context(|| format!("invalid catalog ID {id}"))?;
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
    Ok(entries)
}

fn installed(image: &Image, publisher: &Publisher) -> Result<BTreeMap<String, Installed>> {
    let installed = image.installed()?;
    for (name, package) in &installed {
        check_installed(name, package, publisher)?;
    }
    Ok(installed)
}

fn check_installed(name: &str, package: &Installed, publisher: &Publisher) -> Result<()> {
    let manifest = &package.published.manifest;
    ensure!(
        manifest.name == name && manifest.publisher == publisher.name,
        "invalid installed package identity for {name}"
    );
    package.published.verify()
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
    let publisher = image.publisher()?;
    let installed = installed(&image, &publisher)?;
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
    let mut entries = available(&publisher)?;
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
    let selector = Selector::parse(spec)?;
    let image = Image::open_query(root)?;
    image.require_clean()?;
    let publisher = image.publisher()?;
    ensure!(
        selector.publisher.is_none_or(|name| name == publisher.name),
        "image is configured for publisher {}",
        publisher.name
    );
    let accepts = |version| selector.version.is_none_or(|wanted| wanted == version);
    let package = if from_catalog {
        let entry = available(&publisher)?
            .into_iter()
            .find(|entry| entry.name == selector.name && accepts(entry.version))
            .with_context(|| format!("package is absent from the signed catalog: {spec}"))?;
        read_package(&publisher.repo, &publisher.name, &entry.id, entry.sha256)?
    } else {
        let package = image
            .installed_package(selector.name)?
            .filter(|entry| accepts(entry.published.manifest.version))
            .with_context(|| format!("package is not installed: {spec}"))?;
        check_installed(selector.name, &package, &publisher)?;
        package.published
    };
    Ok(manifest_text(&package.manifest))
}
