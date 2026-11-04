use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};

use super::{Write, requested_names};
use crate::{
    image::{State, root::Root},
    model::Node,
};

pub(super) fn writes(
    root: &Root,
    state: &State,
    packages: &[String],
    preserved: &mut Vec<String>,
) -> Result<BTreeMap<String, Write>> {
    let paths: BTreeSet<&str> = if packages.is_empty() {
        state.paths.keys().map(String::as_str).collect()
    } else {
        let packages = requested_names(packages)?;
        for name in &packages {
            ensure!(
                state.installed.contains_key(name),
                "package is not installed: {name}"
            );
        }
        state
            .paths
            .iter()
            .filter(|(_, managed)| !managed.owners.is_disjoint(&packages))
            .flat_map(|(path, _)| {
                path.match_indices('/')
                    .map(|(index, _)| &path[..index])
                    .chain(std::iter::once(path.as_str()))
            })
            .collect()
    };

    let mut writes = BTreeMap::new();
    for path in paths {
        let managed = &state.paths[path];
        let before = root.inspect(path)?;
        if before.as_ref() == Some(&managed.node)
            || (managed.implicit && before.as_ref().is_some_and(Node::is_dir))
        {
            continue;
        }
        if managed.preserve {
            ensure!(
                matches!(before, None | Some(Node::File { .. })),
                "config type changed at {path}"
            );
            preserved.push(path.to_owned());
            continue;
        }
        if managed.node.is_dir() {
            ensure!(
                before.as_ref().is_none_or(Node::is_dir),
                "path type conflict at {path}"
            );
            if before.is_some() {
                // Check the target can be opened without crossing a mount.
                root.dir(path)?;
            }
        } else {
            ensure!(
                !matches!(before, Some(Node::Dir { .. } | Node::Special)),
                "path type conflict at {path}"
            );
        }
        writes.insert(
            path.to_owned(),
            Write {
                path: path.to_owned(),
                before,
                after: Some(managed.node.clone()),
            },
        );
    }
    Ok(writes)
}
