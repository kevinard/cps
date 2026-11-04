use std::{borrow::Borrow, collections::BTreeMap};

use anyhow::{Context, Result, ensure};

use super::Plan;
use crate::{
    image::State,
    model::{PackageSpec, Version},
    repo::{Published, snapshot},
};

#[derive(Default)]
pub(super) struct Catalogs {
    pub legacy: Option<String>,
    pub pins: BTreeMap<String, String>,
    pub candidates: CandidateSet<Published>,
}

pub(crate) struct CandidateSet<P> {
    pub packages: Vec<P>,
    pub obsoletions: BTreeMap<String, Version>,
}

impl<P> Default for CandidateSet<P> {
    fn default() -> Self {
        Self {
            packages: Vec::new(),
            obsoletions: BTreeMap::new(),
        }
    }
}

impl<P: Borrow<Published>> CandidateSet<P> {
    pub(crate) fn add_obsoletions<'a>(
        &mut self,
        sources: &mut BTreeMap<String, &'a str>,
        publisher: &'a str,
        obsoletions: &BTreeMap<String, Version>,
    ) {
        for (name, cutoff) in obsoletions {
            if *sources.entry(name.clone()).or_insert(publisher) == publisher {
                self.obsoletions.insert(name.clone(), *cutoff);
            }
        }
    }

    pub(crate) fn add_packages<'a>(
        &mut self,
        sources: &mut BTreeMap<String, &'a str>,
        publisher: &'a str,
        packages: impl IntoIterator<Item = P>,
    ) {
        self.packages.extend(packages.into_iter().filter(|pkg| {
            *sources
                .entry(pkg.borrow().manifest.name.clone())
                .or_insert(publisher)
                == publisher
        }));
    }
}

pub(super) fn requested<'a>(
    state: &State,
    packages: &'a [String],
) -> Result<BTreeMap<&'a str, PackageSpec<'a>>> {
    ensure!(!packages.is_empty(), "request requires a package name");
    let mut requested = BTreeMap::new();
    for package in packages {
        let spec = PackageSpec::parse(package)
            .with_context(|| format!("invalid requested package {package:?}"))?;
        if let Some(publisher) = spec.publisher {
            ensure!(
                state.publishers.get(publisher).is_some(),
                "publisher {publisher} is not configured in this image"
            );
            if let Some(installed) = state.installed.get(spec.name) {
                let current = &installed.published.manifest.publisher;
                ensure!(
                    current == publisher,
                    "{} is installed from sticky publisher {current}; remove it before selecting {publisher}",
                    spec.name
                );
            }
        }
        let previous = requested.insert(spec.name, spec);
        ensure!(
            previous.is_none_or(|previous| previous == spec),
            "conflicting selectors for {}",
            spec.name
        );
    }
    Ok(requested)
}

pub(super) fn load(
    state: &State,
    requested: &BTreeMap<&str, PackageSpec<'_>>,
    saved: Option<&Plan>,
) -> Result<Catalogs> {
    let publishers = state.publishers.as_slice();
    let single = publishers.len() == 1;
    if let Some(saved) = saved {
        ensure!(
            if single {
                saved.catalog.is_some() && saved.catalogs.is_empty()
            } else {
                saved.catalog.is_none() && saved.catalogs.len() == publishers.len()
            },
            "saved plan's catalogs do not match the image's publishers"
        );
    }

    // Installed names keep their source. For other names only the first
    // publisher that offers the package contributes candidates to the solver.
    let mut sources: BTreeMap<_, _> = state
        .installed
        .iter()
        .map(|(name, pkg)| (name.clone(), pkg.published.manifest.publisher.as_str()))
        .collect();
    sources.extend(requested.values().filter_map(|spec| {
        spec.publisher
            .map(|publisher| (spec.name.to_owned(), publisher))
    }));
    let mut catalogs = Catalogs::default();
    for publisher in publishers {
        let pin = match saved {
            None => None,
            Some(plan) if single => plan.catalog.as_deref(),
            Some(plan) => Some(
                plan.catalogs
                    .get(&publisher.name)
                    .with_context(|| format!("saved plan has no catalog for {}", publisher.name))?
                    .as_str(),
            ),
        };
        let snapshot = snapshot(&publisher.repo, &publisher.name, &publisher.key, pin)
            .with_context(|| format!("reading signed catalog for {}", publisher.name))?;
        catalogs
            .candidates
            .add_obsoletions(&mut sources, &publisher.name, &snapshot.obsoletions);
        for spec in requested.values() {
            if let Some(version) = spec.version
                && sources.get(spec.name) == Some(&publisher.name.as_str())
                && snapshot
                    .obsoletions
                    .get(spec.name)
                    .is_none_or(|cutoff| version > *cutoff)
            {
                let id = format!("cps://{}/{}@{version}", publisher.name, spec.name);
                ensure!(!snapshot.is_withdrawn(&id), "{id} was withdrawn");
            }
        }
        if single {
            catalogs.legacy = Some(snapshot.sha256);
        } else {
            catalogs
                .pins
                .insert(publisher.name.clone(), snapshot.sha256);
        }
        catalogs
            .candidates
            .add_packages(&mut sources, &publisher.name, snapshot.packages);
    }
    Ok(catalogs)
}
