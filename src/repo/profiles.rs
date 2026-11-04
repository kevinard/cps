use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    path::Path,
};

use anyhow::{Context, Result, ensure};

use crate::{
    boot::kernel,
    image::{Installed, Managed},
    kdl_format::publish_policy::{Companion, Profile},
    model::{Node, PackageSpec, Version},
    plan::{
        CandidateSet, Candidates, SolverRevision, check_directory, check_path_type,
        resolve_packages, select_paths,
    },
};

use super::{Catalog, Published, Snapshot, read_catalog, read_key, read_package, snapshot};

type Composition = (BTreeMap<String, Installed>, BTreeMap<String, Managed>);

struct CompanionCatalogs<'a> {
    name: &'a str,
    current: Snapshot,
    baseline: Option<Snapshot>,
}

impl<'a> CompanionCatalogs<'a> {
    fn load(companion: &'a Companion) -> Result<Self> {
        let config = &companion.config;
        let key = read_key(&config.trust_key)?;
        let load = |hash: &str| {
            snapshot(&config.repository, &config.name, &key, Some(hash))
                .with_context(|| format!("reading companion catalog {hash} for {}", config.name))
        };
        Ok(Self {
            name: &config.name,
            current: load(&companion.catalog)?,
            baseline: companion
                .upgrade_from
                .as_deref()
                .filter(|hash| *hash != companion.catalog)
                .map(load)
                .transpose()?,
        })
    }
}

pub(super) fn check(
    repo: &Path,
    catalog: &Catalog,
    profiles: &BTreeMap<String, Profile>,
    key: &[u8],
) -> Result<()> {
    if profiles.is_empty() {
        return Ok(());
    }
    let packages = load_packages(repo, catalog)?;
    let mut baselines = BTreeMap::new();
    for (name, profile) in profiles {
        let companions = profile
            .companions
            .iter()
            .map(CompanionCatalogs::load)
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("loading companion publishers for release profile {name}"))?;
        let sources = profile
            .packages
            .keys()
            .map(|name| (name.clone(), catalog.publisher.as_str()))
            .collect::<BTreeMap<_, _>>();
        let current = candidates(catalog, &packages, &companions, false, sources.clone());
        let (target, _) = compose(&current, profile, true)
            .with_context(|| format!("checking release profile {name}"))?;
        if let Some(hash) = profile.upgrade_from.as_deref() {
            let (baseline, baseline_packages) = match baselines.entry(hash) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let baseline =
                        read_catalog(repo, hash, &catalog.publisher, key).with_context(|| {
                            format!("reading upgrade catalog {hash} for profile {name}")
                        })?;
                    let packages = load_packages(repo, &baseline)?;
                    entry.insert((baseline, packages))
                }
            };
            (|| {
                let baseline = candidates(baseline, baseline_packages, &companions, true, sources);
                let (before, old_paths) =
                    compose(&baseline, profile, false).context("composing the baseline image")?;
                let sticky = before
                    .iter()
                    .map(|(name, pkg)| (name.clone(), pkg.published.manifest.publisher.as_str()))
                    .collect();
                let current = candidates(catalog, &packages, &companions, false, sticky);
                check_upgrade(&before, old_paths, &current, &target, profile)
            })()
            .with_context(|| format!("checking upgrade from {hash} for release profile {name}"))?;
        }
    }
    Ok(())
}

fn candidates<'a>(
    catalog: &'a Catalog,
    packages: &'a [Published],
    companions: &'a [CompanionCatalogs<'_>],
    baseline: bool,
    mut sources: BTreeMap<String, &'a str>,
) -> CandidateSet<&'a Published> {
    let mut selected = CandidateSet::default();
    selected.add_obsoletions(&mut sources, &catalog.publisher, &catalog.obsoletions);
    selected.add_packages(&mut sources, &catalog.publisher, packages);
    for companion in companions {
        let snapshot = if baseline {
            companion.baseline.as_ref().unwrap_or(&companion.current)
        } else {
            &companion.current
        };
        selected.add_obsoletions(&mut sources, companion.name, &snapshot.obsoletions);
        selected.add_packages(&mut sources, companion.name, &snapshot.packages);
    }
    selected
}

fn load_packages(repo: &Path, catalog: &Catalog) -> Result<Vec<Published>> {
    catalog
        .entries
        .iter()
        .map(|(id, manifest)| read_package(repo, &catalog.publisher, id, manifest.clone()))
        .collect()
}

fn newest_versions<'a>(
    packages: impl IntoIterator<Item = &'a Published>,
) -> BTreeMap<&'a str, Version> {
    let mut newest = BTreeMap::<&str, Version>::new();
    for package in packages {
        let manifest = &package.manifest;
        newest
            .entry(&manifest.name)
            .and_modify(|version| *version = (*version).max(manifest.version))
            .or_insert(manifest.version);
    }
    newest
}

fn compose(
    catalogs: &CandidateSet<&Published>,
    profile: &Profile,
    target_pins: bool,
) -> Result<Composition> {
    let newest = newest_versions(catalogs.packages.iter().copied());
    let mut requested = BTreeMap::new();
    for (name, pin) in &profile.packages {
        let version = pin
            .filter(|_| target_pins)
            .or_else(|| newest.get(name.as_str()).copied())
            .with_context(|| format!("profile package {name} is unavailable"))?;
        requested.insert(
            name.as_str(),
            PackageSpec {
                publisher: None,
                name,
                version: Some(version),
            },
        );
    }
    resolve_packages(
        &BTreeMap::new(),
        Candidates {
            packages: catalogs.packages.iter().copied(),
            obsoletions: &catalogs.obsoletions,
        },
        &requested,
        false,
        true,
        Some(SolverRevision::SatV1),
        |installed| profile_paths(profile, installed),
    )
}

fn profile_paths(
    profile: &Profile,
    installed: &BTreeMap<String, Installed>,
) -> Result<BTreeMap<String, Managed>> {
    let paths = select_paths(
        &profile.arch,
        &profile.facets,
        Node::Dir {
            mode: 0o755,
            uid: 0,
            gid: 0,
        },
        installed,
    )?;
    kernel::selected(installed, &paths)?;
    Ok(paths)
}

fn check_upgrade(
    before: &BTreeMap<String, Installed>,
    old_paths: BTreeMap<String, Managed>,
    catalogs: &CandidateSet<&Published>,
    target: &BTreeMap<String, Installed>,
    profile: &Profile,
) -> Result<()> {
    let (after, new_paths) = resolve_packages(
        before,
        Candidates {
            packages: catalogs.packages.iter().copied(),
            obsoletions: &catalogs.obsoletions,
        },
        &BTreeMap::new(),
        true,
        false,
        Some(SolverRevision::SatV1),
        |installed| profile_paths(profile, installed),
    )?;
    for name in profile.packages.keys() {
        let expected = &target[name].published;
        let selected = &after
            .get(name)
            .with_context(|| format!("update removes profile root {name}"))?
            .published;
        ensure!(
            selected.sha256 == expected.sha256,
            "update selects {} instead of {}",
            selected.manifest.id(),
            expected.manifest.id(),
        );
    }
    let available: BTreeSet<_> = catalogs.packages.iter().map(|pkg| &pkg.sha256).collect();
    for package in after.values() {
        let id = package.published.manifest.id();
        ensure!(
            available.contains(&package.published.sha256),
            "update retains unavailable package {id}"
        );
    }
    kernel::transition(
        kernel::selected(before, &old_paths)?,
        kernel::selected(&after, &new_paths)?,
    )?;
    for (path, old) in old_paths {
        if let Some(new) = new_paths.get(&path) {
            check_path_type(&path, &old, new)?;
            if new.node.is_dir() && &old != new {
                check_directory(&path, &old.node, new)?;
            }
        }
    }
    Ok(())
}
