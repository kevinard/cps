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
    pub path: String,
    pub owners: [String; 2],
}

impl PathConflict {
    fn new(path: &str, first: &str, second: &str) -> Self {
        Self {
            path: path.to_owned(),
            owners: [first.to_owned(), second.to_owned()],
        }
    }
}

impl fmt::Display for PathConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [first, second] = &self.owners;
        if first == second {
            write!(f, "{first} selects incompatible actions at {}", self.path)
        } else {
            write!(
                f,
                "path conflict for {} between {first} and {second}",
                self.path
            )
        }
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
    implicit_directory: Node,
    installed: &BTreeMap<String, Installed>,
) -> Result<BTreeMap<String, Managed>> {
    let mut paths: BTreeMap<String, Managed> = BTreeMap::new();
    let mut sizes = BTreeMap::new();
    for (name, package) in installed {
        let mut selected = BTreeSet::new();
        for action in &package.published.manifest.action {
            let (facet, action_arch) = action.selectors();
            if facet.is_some_and(|key| !facets.get(key).copied().unwrap_or(true))
                || action_arch.is_some_and(|action_arch| action_arch != arch)
            {
                continue;
            }
            let path = action.path();
            if !selected.insert(path) {
                return Err(PathConflict::new(path, name, name).into());
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
