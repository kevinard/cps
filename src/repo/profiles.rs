use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result};

use crate::{
    boot::kernel,
    kdl_format::publish_policy::Profile,
    model::{Node, PackageSpec, Version},
    plan::{SolverRevision, resolve_packages, select_paths},
};

use super::{Catalog, Published, read_package};

pub(super) fn check(
    repo: &Path,
    catalog: &Catalog,
    profiles: &BTreeMap<String, Profile>,
) -> Result<()> {
    if profiles.is_empty() {
        return Ok(());
    }
    let packages = catalog
        .entries
        .iter()
        .map(|(id, manifest)| read_package(repo, &catalog.publisher, id, manifest.clone()))
        .collect::<Result<Vec<_>>>()?;
    let mut newest = BTreeMap::<&str, Version>::new();
    for package in &packages {
        let manifest = &package.manifest;
        newest
            .entry(&manifest.name)
            .and_modify(|version| *version = (*version).max(manifest.version))
            .or_insert(manifest.version);
    }
    for (name, profile) in profiles {
        compose(&packages, &newest, profile)
            .with_context(|| format!("checking release profile {name}"))?;
    }
    Ok(())
}

fn compose(
    packages: &[Published],
    newest: &BTreeMap<&str, Version>,
    profile: &Profile,
) -> Result<()> {
    let mut requested = BTreeMap::new();
    for (name, pin) in &profile.packages {
        let version = pin
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
        packages,
        &requested,
        false,
        true,
        Some(SolverRevision::SatV1),
        |installed| {
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
            Ok(())
        },
    )?;
    Ok(())
}
