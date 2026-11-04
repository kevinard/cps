use std::{
    collections::{BTreeMap, btree_map::Entry},
    path::Path,
};

use anyhow::{Context, Result, ensure};

use crate::{
    boot::kernel,
    image::{Installed, Managed},
    kdl_format::publish_policy::Profile,
    model::{Node, PackageSpec, Version},
    plan::{
        Candidates, SolverRevision, check_directory, check_path_type, resolve_packages,
        select_paths,
    },
};

use super::{Catalog, Published, read_catalog, read_package};

type Composition = (BTreeMap<String, Installed>, BTreeMap<String, Managed>);

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
    let newest = newest_versions(&packages);
    let mut baselines = BTreeMap::new();
    for (name, profile) in profiles {
        let (target, _) = compose(&packages, &newest, profile, true)
            .with_context(|| format!("checking release profile {name}"))?;
        if let Some(hash) = profile.upgrade_from.as_deref() {
            let baseline = match baselines.entry(hash) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let baseline =
                        read_catalog(repo, hash, &catalog.publisher, key).with_context(|| {
                            format!("reading upgrade catalog {hash} for profile {name}")
                        })?;
                    entry.insert(load_packages(repo, &baseline)?)
                }
            };
            check_upgrade(baseline, catalog, &packages, &target, profile).with_context(|| {
                format!("checking upgrade from {hash} for release profile {name}")
            })?;
        }
    }
    Ok(())
}

fn load_packages(repo: &Path, catalog: &Catalog) -> Result<Vec<Published>> {
    catalog
        .entries
        .iter()
        .map(|(id, manifest)| read_package(repo, &catalog.publisher, id, manifest.clone()))
        .collect()
}

fn newest_versions(packages: &[Published]) -> BTreeMap<&str, Version> {
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
    packages: &[Published],
    newest: &BTreeMap<&str, Version>,
    profile: &Profile,
    target_pins: bool,
) -> Result<Composition> {
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
            packages,
            obsoletions: &BTreeMap::new(),
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
    baseline: &[Published],
    catalog: &Catalog,
    packages: &[Published],
    target: &BTreeMap<String, Installed>,
    profile: &Profile,
) -> Result<()> {
    let (before, old_paths) = compose(baseline, &newest_versions(baseline), profile, false)
        .context("composing the baseline image")?;
    let (after, new_paths) = resolve_packages(
        &before,
        Candidates {
            packages,
            obsoletions: &catalog.obsoletions,
        },
        &BTreeMap::new(),
        true,
        false,
        Some(SolverRevision::SatV1),
        |installed| profile_paths(profile, installed),
    )?;
    for name in profile.packages.keys() {
        let expected = &target[name].published;
        let selected = &after[name].published;
        ensure!(
            selected.sha256 == expected.sha256,
            "update selects {} instead of {}",
            selected.manifest.id(),
            expected.manifest.id(),
        );
    }
    for package in after.values() {
        let id = package.published.manifest.id();
        ensure!(
            catalog.entries.get(&id) == Some(&package.published.sha256),
            "update retains unavailable package {id}"
        );
    }
    kernel::transition(
        kernel::selected(&before, &old_paths)?,
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
