pub(crate) mod elf;
pub(crate) mod runtime;

use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    path::Path,
};

use anyhow::{Context, Result, ensure};

use crate::{
    boot::kernel,
    image::{Installed, Managed},
    kdl_format::publish_policy::{Companion, Profile},
    model::{Node, PackageSpec, Selection, Version, VersionConstraint},
    plan::{
        CandidateSet, Candidates, Renames, Retention, check_directory, check_path_type,
        resolve_packages, select_paths,
    },
};

use super::{
    Catalog, CatalogLineage, CatalogPin, Published, ReleaseAdvisory, ReleaseProfile, Snapshot,
    catalog_snapshot, read_catalog, read_key, read_package, snapshot,
};

type Composition = (BTreeMap<String, Installed>, BTreeMap<String, Managed>);

struct CompanionCatalogs<'a> {
    name: &'a str,
    repo: &'a Path,
    key: Vec<u8>,
    pin: CatalogPin,
    current: Snapshot,
    upgrade_from: Option<&'a str>,
}

impl<'a> CompanionCatalogs<'a> {
    fn load(
        companion: &'a Companion,
        previous: Option<&CatalogPin>,
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
            key,
            pin: CatalogPin {
                publisher: config.name.clone(),
                sha256: companion.catalog.clone(),
                build_trust: trust,
                lineage: Some(lineage),
            },
            current,
            upgrade_from: companion.upgrade_from.as_deref(),
        })
    }

    fn baseline(&self, signed: Option<&CatalogPin>, explicit: bool) -> Result<Option<Snapshot>> {
        let override_hash = self.upgrade_from.filter(|_| explicit);
        if let Some(pin) = signed {
            ensure!(
                override_hash.is_none_or(|hash| hash == pin.sha256),
                "companion upgrade-from for {} differs from the signed baseline pin {}",
                self.name,
                pin.sha256
            );
            if pin.sha256 == self.pin.sha256 {
                ensure!(
                    pin.lineage.is_none() || pin.lineage == self.pin.lineage,
                    "signed companion lineage differs from catalog {} for {}",
                    pin.sha256,
                    self.name
                );
                return Ok(None);
            }
            return pin
                .snapshot(self.repo, self.name, &self.key)
                .with_context(|| {
                    format!(
                        "reading baseline companion catalog {} for {}",
                        pin.sha256, self.name
                    )
                })
                .map(Some);
        }
        override_hash
            .filter(|hash| *hash != self.pin.sha256)
            .map(|hash| {
                snapshot(self.repo, self.name, &self.key, Some(hash))
                    .with_context(|| format!("reading companion catalog {hash} for {}", self.name))
            })
            .transpose()
    }
}

pub(super) fn check(
    repo: &Path,
    catalog: &Catalog,
    current_hash: Option<&str>,
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
        let previous = catalog.release_profiles.get(name);
        let companions = profile
            .companions
            .iter()
            .map(|companion| {
                let previous = previous.and_then(|profile| {
                    profile
                        .companions
                        .iter()
                        .find(|pin| pin.publisher == companion.config.name)
                });
                CompanionCatalogs::load(companion, previous, &primary_keys)
            })
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("loading companion publishers for release profile {name}"))?;
        let sources = profile
            .packages
            .keys()
            .map(|name| (name.clone(), catalog.publisher.as_str()))
            .collect::<BTreeMap<_, _>>();
        let current = candidates(
            catalog,
            &packages,
            &profile.arch,
            companions
                .iter()
                .map(|companion| (companion.name, &companion.current)),
            sources,
        );
        let (target, paths) = compose(
            &current,
            profile,
            profile
                .packages
                .iter()
                .map(|(name, version)| (name.as_str(), version.as_ref())),
        )
        .with_context(|| format!("checking release profile {name}"))?;
        check_companion_builds(&companions, &target)
            .with_context(|| format!("checking companion builds for release profile {name}"))?;
        runtime::check(repo, &catalog.publisher, profile, &target, &paths)
            .with_context(|| format!("checking ELF closure for release profile {name}"))?;
        let next = ReleaseProfile {
            architecture: profile.arch.clone(),
            facets: profile.facets.clone(),
            variants: profile.variants.clone(),
            facet_overrides: profile.facet_overrides.clone(),
            roots: profile
                .packages
                .keys()
                .map(|name| {
                    (
                        name.clone(),
                        target[name].published.manifest.version.clone(),
                    )
                })
                .collect(),
            composition: target
                .iter()
                .map(|(name, package)| (name.clone(), package.published.sha256.clone()))
                .collect(),
            companions: companions
                .iter()
                .map(|companion| companion.pin.clone())
                .collect(),
            elf_library_dirs: profile.elf_library_dirs.clone(),
            elf_symbol_versions: !profile.elf_library_dirs.is_empty(),
            elf_require_declarations: profile.elf_require_declarations,
            rollout_percentage: profile.rollout_percentage,
            advisories: check_advisories(name, profile, previous, &target)?,
        };
        let explicit = profile.upgrade_from.as_deref();
        let immediate = current_hash.filter(|hash| {
            Some(*hash) != explicit
                && !profile
                    .signed_baselines
                    .iter()
                    .any(|baseline| baseline == hash)
                && previous.is_some()
        });
        for (hash, explicit, require_signed_composition) in explicit
            .map(|hash| (hash, true, false))
            .into_iter()
            .chain(
                profile
                    .signed_baselines
                    .iter()
                    .map(|hash| (hash.as_str(), false, true)),
            )
            .chain(immediate.map(|hash| (hash, false, false)))
        {
            (|| {
                let (baseline, baseline_packages) = match baselines.entry(hash) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        let baseline = read_catalog(repo, hash, &catalog.publisher, key)?;
                        let packages = load_packages(repo, &baseline)?;
                        entry.insert((baseline, packages))
                    }
                };
                let historical = baseline.release_profiles.get(name);
                ensure!(
                    !require_signed_composition || historical.is_some(),
                    "baseline catalog has no signed composition for profile {name}"
                );
                let (before, old_paths) = baseline_image(
                    baseline,
                    baseline_packages,
                    historical,
                    profile,
                    &companions,
                    explicit,
                )?;
                let mut sticky = before
                    .iter()
                    .map(|(name, pkg)| (name.clone(), pkg.published.manifest.publisher.as_str()))
                    .collect();
                let mut renames = Renames::default();
                renames.add(
                    &before,
                    &catalog.publisher,
                    &catalog.rename_targets()?,
                    &catalog.obsoletions,
                );
                for companion in &companions {
                    renames.add(
                        &before,
                        companion.name,
                        &companion.current.rename_targets,
                        &companion.current.obsoletions,
                    );
                }
                renames.reserve_sources(&mut sticky)?;
                let current = candidates(
                    catalog,
                    &packages,
                    &profile.arch,
                    companions
                        .iter()
                        .map(|companion| (companion.name, &companion.current)),
                    sticky,
                );
                let (after, paths) = check_upgrade(
                    &before,
                    old_paths,
                    &current,
                    &target,
                    &next,
                    historical.is_some(),
                    &renames,
                )?;
                check_companion_builds(&companions, &after)?;
                runtime::check(repo, &catalog.publisher, profile, &after, &paths)
            })()
            .with_context(|| format!("checking upgrade from {hash} for release profile {name}"))?;
        }
        checked.insert(name.clone(), next);
    }
    Ok(checked)
}

fn check_advisories(
    name: &str,
    policy: &Profile,
    previous: Option<&ReleaseProfile>,
    target: &BTreeMap<String, Installed>,
) -> Result<Vec<ReleaseAdvisory>> {
    let mut advisories = BTreeMap::new();
    for advisory in previous.into_iter().flat_map(|profile| &profile.advisories) {
        if let Some(package) = target.get(&advisory.package)
            && package.published.manifest.publisher == advisory.publisher
        {
            let mut advisory = advisory.clone();
            advisory.fixed = package.published.manifest.version.clone();
            advisories.insert((advisory.id.clone(), advisory.package.clone()), advisory);
        }
    }
    for advisory in &policy.advisories {
        let package = &target
            .get(&advisory.package)
            .with_context(|| {
                format!(
                    "advisory {} package {} is absent from profile {name}",
                    advisory.id, advisory.package
                )
            })?
            .published
            .manifest;
        advisories.insert(
            (advisory.id.clone(), advisory.package.clone()),
            ReleaseAdvisory {
                id: advisory.id.clone(),
                package: advisory.package.clone(),
                publisher: package.publisher.clone(),
                affected: advisory.affected.clone(),
                fixed: package.version.clone(),
                summary: advisory.summary.clone(),
                action: advisory.action.clone(),
            },
        );
    }
    for advisory in advisories.values() {
        ensure!(
            !VersionConstraint::parse(Some(&advisory.affected))?.matches(&advisory.fixed),
            "advisory {} affects the selected version of {} in profile {name}",
            advisory.id,
            advisory.package
        );
    }
    Ok(advisories.into_values().collect())
}

fn baseline_image(
    catalog: &Catalog,
    packages: &[Published],
    historical: Option<&ReleaseProfile>,
    profile: &Profile,
    companions: &[CompanionCatalogs<'_>],
    explicit: bool,
) -> Result<Composition> {
    if let Some(historical) = historical {
        ensure!(
            historical.architecture == profile.arch
                && historical.facets == profile.facets
                && historical.variants == profile.variants
                && historical
                    .companions
                    .iter()
                    .map(|pin| pin.publisher.as_str())
                    .eq(companions.iter().map(|companion| companion.name)),
            "image configuration differs from the signed baseline"
        );
    }
    let snapshots = companions
        .iter()
        .enumerate()
        .map(|(index, companion)| {
            companion.baseline(
                historical.map(|profile| &profile.companions[index]),
                explicit,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let snapshots = companions
        .iter()
        .zip(&snapshots)
        .map(|(companion, snapshot)| {
            (
                companion.name,
                snapshot.as_ref().unwrap_or(&companion.current),
            )
        });
    let roots = match historical {
        Some(historical) => historical
            .roots
            .iter()
            .map(|(name, version)| (name.as_str(), Some(version)))
            .collect::<Vec<_>>(),
        None => profile
            .packages
            .keys()
            .map(|name| (name.as_str(), None))
            .collect(),
    };
    let sources = roots
        .iter()
        .map(|(name, _)| ((*name).to_owned(), catalog.publisher.as_str()))
        .collect();
    let baseline = candidates(catalog, packages, &profile.arch, snapshots, sources);
    if let Some(historical) = historical {
        let installed = baseline
            .replay(historical)
            .context("replaying the baseline image")?;
        let paths = profile_paths(
            Selection::image(&profile.arch, &profile.variants, &profile.facets),
            &installed,
        )?;
        return Ok((installed, paths));
    }
    compose(&baseline, profile, roots).context("composing the baseline image")
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
    architecture: &str,
    companions: impl IntoIterator<Item = (&'a str, &'a Snapshot)>,
    mut sources: BTreeMap<String, &'a str>,
) -> CandidateSet<&'a Published> {
    let mut selected = CandidateSet::default();
    selected.add_obsoletions(&mut sources, &catalog.publisher, &catalog.obsoletions);
    selected.add_packages(&mut sources, &catalog.publisher, architecture, packages);
    for (name, snapshot) in companions {
        selected.add_obsoletions(&mut sources, name, &snapshot.obsoletions);
        selected.add_packages(&mut sources, name, architecture, &snapshot.packages);
    }
    selected
}

fn load_packages(repo: &Path, catalog: &Catalog) -> Result<Vec<Published>> {
    catalog
        .entries
        .iter()
        .map(|(id, entry)| read_package(repo, &catalog.publisher, id, entry))
        .collect()
}

fn newest_versions<'a>(
    packages: impl IntoIterator<Item = &'a Published>,
) -> BTreeMap<&'a str, &'a Version> {
    let mut newest = BTreeMap::<&str, &Version>::new();
    for package in packages {
        let manifest = &package.manifest;
        newest
            .entry(&manifest.name)
            .and_modify(|version| *version = (*version).max(&manifest.version))
            .or_insert(&manifest.version);
    }
    newest
}

fn compose<'a>(
    catalogs: &CandidateSet<&Published>,
    profile: &Profile,
    roots: impl IntoIterator<Item = (&'a str, Option<&'a Version>)>,
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
                version: Some(version.clone()),
            },
        );
    }
    resolve_packages(
        &BTreeMap::new(),
        Candidates {
            packages: catalogs.packages.iter().copied(),
            obsoletions: &catalogs.obsoletions,
            freezes: &BTreeMap::new(),
            profile_packages: &BTreeMap::new(),
        },
        &requested,
        false,
        Retention::None,
        true,
        |installed| {
            profile_paths(
                Selection::image(&profile.arch, &profile.variants, &profile.facets),
                installed,
            )
        },
    )
}

/// Compose a signed profile's paths. Profiles carry image-wide facets and variants only;
/// per-package facet rules belong to individual images.
pub(super) fn profile_paths(
    selection: Selection<'_>,
    installed: &BTreeMap<String, Installed>,
) -> Result<BTreeMap<String, Managed>> {
    let paths = select_paths(
        selection,
        &BTreeMap::new(),
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
    profile: &ReleaseProfile,
    locked: bool,
    renames: &Renames,
) -> Result<Composition> {
    let compose = |installed: &_| {
        profile_paths(
            Selection::image(&profile.architecture, &profile.variants, &profile.facets),
            installed,
        )
    };
    let successors = renames.requirements();
    let (after, new_paths) = if locked {
        renames.check_profile(|_| true, Some(profile))?;
        catalogs.update_profile(
            before,
            profile,
            &BTreeMap::new(),
            Some(&BTreeSet::new()),
            &successors,
            true,
            compose,
        )?
    } else {
        let result = resolve_packages(
            before,
            Candidates {
                packages: catalogs.packages.iter().copied(),
                obsoletions: &catalogs.obsoletions,
                freezes: &BTreeMap::new(),
                profile_packages: &BTreeMap::new(),
            },
            &successors,
            true,
            Retention::All,
            true,
            compose,
        )?;
        for name in profile.roots.keys() {
            let expected = &target[name].published;
            let selected = &result
                .0
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
        result
    };
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
