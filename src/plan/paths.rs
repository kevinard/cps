use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    fmt,
};

use anyhow::{Result, ensure};

use crate::{
    image::{Installed, Managed, Settings},
    model::Node,
};

#[derive(Debug)]
pub(super) struct PathConflict {
    message: String,
    pub owners: [String; 2],
}

impl PathConflict {
    fn new(path: &str, first: &str, second: &str) -> Self {
        let message = if first == second {
            format!("{first} selects incompatible actions at {path}")
        } else {
            format!("path conflict for {path} between {first} and {second}")
        };
        Self {
            message,
            owners: [first.to_owned(), second.to_owned()],
        }
    }
}

impl fmt::Display for PathConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PathConflict {}

pub(crate) fn effective_paths(
    settings: &Settings,
    installed: &BTreeMap<String, Installed>,
) -> Result<BTreeMap<String, Managed>> {
    select_paths(
        &settings.arch,
        &settings.facets,
        &settings.mediators,
        settings.directory(),
        installed,
    )
}

pub(crate) fn check_path_type(path: &str, before: &Managed, after: &Managed) -> Result<()> {
    ensure!(
        before.node.is_dir() == after.node.is_dir(),
        "cannot change between directory and leaf actions at {path}"
    );
    Ok(())
}

pub(crate) fn check_directory(path: &str, existing: &Node, target: &Managed) -> Result<()> {
    ensure!(
        existing == &target.node || (target.implicit && existing.is_dir()),
        "directory or metadata conflict at {path}"
    );
    Ok(())
}

pub(crate) fn hardlink_groups(paths: &BTreeMap<String, Managed>) -> BTreeMap<&str, Vec<&str>> {
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for (path, managed) in paths {
        if let Node::Hardlink { target } = &managed.node {
            groups
                .entry(target.as_str())
                .or_default()
                .push(path.as_str());
        }
    }
    groups
}

pub(crate) fn select_paths(
    arch: &str,
    facets: &BTreeMap<String, bool>,
    preferences: &BTreeMap<String, String>,
    implicit_directory: Node,
    installed: &BTreeMap<String, Installed>,
) -> Result<BTreeMap<String, Managed>> {
    let offers = mediator_offers(arch, facets, installed);
    let winners = choose_mediators(&offers, preferences);
    let mut paths: BTreeMap<String, Managed> = BTreeMap::new();
    let mut sizes = BTreeMap::new();
    let mut config_names = BTreeMap::new();
    let mut shared_paths = BTreeMap::<&str, (&str, &str)>::new();
    for (name, package) in installed {
        package.published.manifest.check_architecture(arch)?;
        let publisher = &package.published.manifest.publisher;
        let mut selected = BTreeSet::new();
        for action in &package.published.manifest.action {
            if !action.applies_to(arch, facets) {
                continue;
            }
            let path = action.path();
            if !selected.insert(path) {
                return Err(PathConflict::new(path, name, name).into());
            }
            if let Some(link) = package.published.manifest.mediated_links.get(path) {
                if let Some(&(group, owner)) = shared_paths.get(path)
                    && group != link.group
                {
                    return Err(PathConflict::new(path, owner, name).into());
                }
                shared_paths.entry(path).or_insert((&link.group, name));
                if winners[link.group.as_str()] != name {
                    continue;
                }
            }
            let original_name = action.original_name();
            if action.preserve() {
                let identity = config_name(name, path, original_name);
                if let Some((owner, previous)) =
                    config_names.insert((publisher, identity.clone()), (name, path))
                {
                    return Err(PathConflict {
                        message: format!(
                            "configuration identity {publisher}/{identity} is selected at both {previous} and {path}"
                        ),
                        owners: [owner.clone(), name.clone()],
                    }
                    .into());
                }
            }
            let node = action.node()?;
            if let Node::File { sha256, size, .. } = &node {
                let previous = sizes.entry(sha256.clone()).or_insert(*size);
                ensure!(
                    *previous == *size,
                    "inconsistent payload sizes for {sha256}"
                );
            }
            match paths.entry(path.to_owned()) {
                Entry::Vacant(entry) => {
                    entry.insert(Managed {
                        node,
                        owners: BTreeSet::from([name.clone()]),
                        preserve: action.preserve(),
                        implicit: false,
                        original_name: original_name.map(str::to_owned),
                    });
                }
                Entry::Occupied(mut entry) => {
                    let existing = entry.get_mut();
                    if !node.is_dir() || existing.node != node {
                        let owner = existing.owners.first().expect("explicit path has an owner");
                        return Err(PathConflict::new(path, owner, name).into());
                    }
                    existing.owners.insert(name.clone());
                }
            }
        }
    }
    let mut parents = BTreeMap::new();
    for (path, managed) in &paths {
        let owner = managed.owners.first().expect("explicit path has an owner");
        for (index, _) in path.match_indices('/') {
            let parent = &path[..index];
            if let Some(existing) = paths.get(parent) {
                if !existing.node.is_dir() {
                    let parent_owner = existing.owners.first().expect("explicit path has an owner");
                    return Err(PathConflict::new(parent, parent_owner, owner).into());
                }
            } else {
                parents.entry(parent).or_insert_with(|| Managed {
                    node: implicit_directory.clone(),
                    owners: BTreeSet::new(),
                    preserve: false,
                    implicit: true,
                    original_name: None,
                });
            }
        }
    }
    let parents: Vec<_> = parents
        .into_iter()
        .map(|(path, entry)| (path.to_owned(), entry))
        .collect();
    paths.extend(parents);
    Ok(paths)
}

pub(crate) fn mediator_offers<'a>(
    arch: &str,
    facets: &BTreeMap<String, bool>,
    installed: &'a BTreeMap<String, Installed>,
) -> BTreeMap<&'a str, BTreeMap<&'a str, i32>> {
    let mut offers = BTreeMap::<_, BTreeMap<_, _>>::new();
    for (name, package) in installed {
        let manifest = &package.published.manifest;
        if manifest.mediated_links.is_empty() {
            continue;
        }
        for action in &manifest.action {
            if action.applies_to(arch, facets)
                && let Some(link) = manifest.mediated_links.get(action.path())
            {
                offers
                    .entry(link.group.as_str())
                    .or_default()
                    .insert(name.as_str(), link.priority);
            }
        }
    }
    offers
}

pub(crate) fn choose_mediators<'a>(
    offers: &BTreeMap<&'a str, BTreeMap<&'a str, i32>>,
    preferences: &BTreeMap<String, String>,
) -> BTreeMap<&'a str, &'a str> {
    offers
        .iter()
        .filter_map(|(&group, offers)| {
            let preferred = preferences.get(group).map(String::as_str);
            offers
                .iter()
                .max_by_key(|&(&name, &priority)| {
                    (Some(name) == preferred, priority, std::cmp::Reverse(name))
                })
                .map(|(&name, _)| (group, name))
        })
        .collect()
}

pub(super) fn config_name(package: &str, path: &str, original: Option<&str>) -> String {
    original.map_or_else(|| format!("{package}:{path}"), str::to_owned)
}
