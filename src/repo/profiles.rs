mod elf;
mod runtime;

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

use super::{
    Catalog, CatalogLineage, CatalogPin, Published, ReleaseProfile, Snapshot, catalog_snapshot,
    read_catalog, read_key, read_package, snapshot,
};

type Composition = (BTreeMap<String, Installed>, BTreeMap<String, Managed>);

struct CompanionCatalogs<'a> {
    name: &'a str,
    repo: &'a Path,
    pin: CatalogPin,
    current: Snapshot,
    baseline: Option<Snapshot>,
}

impl<'a> CompanionCatalogs<'a> {
    fn load(
        companion: &'a Companion,
        previous: Option<&CatalogPin>,
        baseline: Option<&CatalogPin>,
        primary_keys: &[[u8; 32]],
    ) -> Result<Self> {
        let config = &companion.config;
        let key = read_key(&config.trust_key)?;
        let catalog = read_catalog(&config.repository, &companion.catalog, &config.name, &key)
            .with_context(|| {
                format!(
                    "reading companion catalog {} for {}",
                    companion.catalog, config.name
                )
            })?;
        let lineage = CatalogLineage::new(&key, &catalog);
        let baseline = if let Some(pin) = baseline {
            ensure!(
                companion
                    .upgrade_from
                    .as_ref()
                    .is_none_or(|hash| *hash == pin.sha256),
                "companion upgrade-from for {} differs from the signed baseline pin {}",
                config.name,
                pin.sha256
            );
            if pin.sha256 == companion.catalog {
                pin.check_lineage(&catalog, &key)?;
                None
            } else {
                Some(
                    pin.snapshot(&config.repository, &config.name, &key)
                        .with_context(|| {
                            format!(
                                "reading baseline companion catalog {} for {}",
                                pin.sha256, config.name
                            )
                        })?,
                )
            }
        } else {
            companion
                .upgrade_from
                .as_deref()
                .filter(|hash| *hash != companion.catalog)
                .map(|hash| {
                    snapshot(&config.repository, &config.name, &key, Some(hash)).with_context(
                        || format!("reading companion catalog {hash} for {}", config.name),
                    )
                })
                .transpose()?
        };
        if let Some(previous) = previous {
            if let Some(old) = &previous.lineage {
                ensure!(
                    old.anchor_sha256 == lineage.anchor_sha256,
                    "release changes companion trust anchor for {}",
                    config.name
                );
            }
            if previous.sha256 != companion.catalog {
                let baseline = previous
                    .read(&config.repository, &config.name, &key)
                    .with_context(|| {
                        format!(
                            "reading previously pinned companion catalog {} for {}",
                            previous.sha256, config.name
                        )
                    })?;
                ensure!(
                    catalog.sequence.is_some(),
                    "new companion catalog for {} has no sequence",
                    config.name
                );
                baseline
                    .check_successor(&catalog, &previous.sha256)
                    .with_context(|| {
                        format!("checking new companion catalog for {}", config.name)
                    })?;
            }
        }
        let current =
            catalog_snapshot(&config.repository, catalog, companion.catalog.clone(), &key)?;
        let trust = config
            .build_trust
            .as_ref()
            .map(|trust| trust.load(&key))
            .transpose()
            .with_context(|| format!("loading build trust for {}", config.name))?;
        if let Some(trust) = &trust {
            current.builds.check_keys(trust)?;
            for key in primary_keys {
                trust.check_publisher_key(key)?;
            }
        }
        Ok(Self {
            name: &config.name,
            repo: &config.repository,
            pin: CatalogPin {
                publisher: config.name.clone(),
                sha256: companion.catalog.clone(),
                build_trust: trust,
                lineage: Some(lineage),
            },
            current,
            baseline,
        })
    }
}

pub(super) fn check(
    repo: &Path,
    catalog: &Catalog,
    profiles: &BTreeMap<String, Profile>,
    key: &[u8],
) -> Result<BTreeMap<String, ReleaseProfile>> {
    if profiles.is_empty() {
        return Ok(BTreeMap::new());
    }
    let packages = load_packages(repo, catalog)?;
    let primary_keys = catalog.publisher_keys(key)?;
    let mut baselines = BTreeMap::new();
    let mut checked = BTreeMap::new();
    for (name, profile) in profiles {
        let baseline = profile
            .upgrade_from
            .as_deref()
            .map(|hash| {
                let stored = match baselines.entry(hash) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        let baseline = read_catalog(repo, hash, &catalog.publisher, key)?;
                        let packages = load_packages(repo, &baseline)?;
                        entry.insert((baseline, packages))
                    }
                };
                Ok::<_, anyhow::Error>((hash, &stored.0, &stored.1))
            })
            .transpose()
            .with_context(|| format!("reading upgrade catalog for profile {name}"))?;
        let historical = baseline.and_then(|(_, baseline, _)| baseline.release_profiles.get(name));
        if let Some(historical) = historical {
            ensure!(
                historical.architecture == profile.arch
                    && historical.facets == profile.facets
                    && historical
                        .companions
                        .iter()
                        .map(|pin| &pin.publisher)
                        .eq(profile
                            .companions
                            .iter()
                            .map(|companion| &companion.config.name)),
                "image configuration differs from the signed baseline for release profile {name}"
            );
        }
        let previous = catalog.release_profiles.get(name);
        let companions = profile
            .companions
            .iter()
            .enumerate()
            .map(|(index, companion)| {
                let previous = previous.and_then(|profile| {
                    profile
                        .companions
                        .iter()
                        .find(|pin| pin.publisher == companion.config.name)
                });
                let baseline = historical.map(|profile| &profile.companions[index]);
                CompanionCatalogs::load(companion, previous, baseline, &primary_keys)
            })
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("loading companion publishers for release profile {name}"))?;
        let sources = profile
            .packages
            .keys()
            .map(|name| (name.clone(), catalog.publisher.as_str()))
            .collect::<BTreeMap<_, _>>();
        let current = candidates(catalog, &packages, &companions, false, sources);
        let (target, paths) = compose(
            &current,
            profile,
            profile
                .packages
                .iter()
                .map(|(name, version)| (name.as_str(), *version)),
        )
        .with_context(|| format!("checking release profile {name}"))?;
        check_companion_builds(&companions, &target)
            .with_context(|| format!("checking companion builds for release profile {name}"))?;
        runtime::check(repo, &catalog.publisher, profile, &target, &paths)
            .with_context(|| format!("checking ELF closure for release profile {name}"))?;
        if let Some((hash, baseline, baseline_packages)) = baseline {
            (|| {
                let roots = match historical.filter(|profile| !profile.roots.is_empty()) {
                    Some(historical) => historical
                        .roots
                        .iter()
                        .map(|(name, version)| (name.as_str(), Some(*version)))
                        .collect::<Vec<_>>(),
                    None => profile
                        .packages
                        .keys()
                        .map(|name| (name.as_str(), None))
                        .collect(),
                };
                let sources = roots
                    .iter()
                    .map(|(name, _)| ((*name).to_owned(), baseline.publisher.as_str()))
                    .collect();
                let baseline = candidates(baseline, baseline_packages, &companions, true, sources);
                let (before, old_paths) = if let Some(historical) =
                    historical.filter(|profile| !profile.composition.is_empty())
                {
                    let installed = baseline
                        .replay(historical)
                        .context("replaying the baseline image")?;
                    let paths = profile_paths(profile, &installed)?;
                    (installed, paths)
                } else {
                    compose(&baseline, profile, roots).context("composing the baseline image")?
                };
                let sticky = before
                    .iter()
                    .map(|(name, pkg)| (name.clone(), pkg.published.manifest.publisher.as_str()))
                    .collect();
                let current = candidates(catalog, &packages, &companions, false, sticky);
                let (after, paths) = check_upgrade(&before, old_paths, &current, &target, profile)?;
                check_companion_builds(&companions, &after)?;
                runtime::check(repo, &catalog.publisher, profile, &after, &paths)
            })()
            .with_context(|| format!("checking upgrade from {hash} for release profile {name}"))?;
        }
        checked.insert(
            name.clone(),
            ReleaseProfile {
                architecture: profile.arch.clone(),
                facets: profile.facets.clone(),
                roots: profile
                    .packages
                    .keys()
                    .map(|name| (name.clone(), target[name].published.manifest.version))
                    .collect(),
                composition: target
                    .into_iter()
                    .map(|(name, package)| (name, package.published.sha256))
                    .collect(),
                companions: companions
                    .into_iter()
                    .map(|companion| companion.pin)
                    .collect(),
                elf_library_dirs: profile.elf_library_dirs.clone(),
                elf_symbol_versions: !profile.elf_library_dirs.is_empty(),
            },
        );
    }
    Ok(checked)
}

fn check_companion_builds(
    companions: &[CompanionCatalogs<'_>],
    installed: &BTreeMap<String, Installed>,
) -> Result<()> {
    for companion in companions {
        let Some(trust) = &companion.pin.build_trust else {
            continue;
        };
        for package in installed
            .values()
            .filter(|pkg| pkg.published.manifest.publisher == companion.name)
        {
            companion
                .current
                .builds
                .verify(companion.repo, [trust], &package.published)?;
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

fn compose<'a>(
    catalogs: &CandidateSet<&Published>,
    profile: &Profile,
    roots: impl IntoIterator<Item = (&'a str, Option<Version>)>,
) -> Result<Composition> {
    let newest = newest_versions(catalogs.packages.iter().copied());
    let mut requested = BTreeMap::new();
    for (name, pin) in roots {
        let version = pin
            .or_else(|| newest.get(name).copied())
            .with_context(|| format!("profile package {name} is unavailable"))?;
        requested.insert(
            name,
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
) -> Result<Composition> {
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
    Ok((after, new_paths))
}
