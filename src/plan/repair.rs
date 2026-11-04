use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};

use super::{Write, config::with_vendor_metadata, requested_names};
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
        if let Node::Hardlink { target } = &managed.node
            && root.hardlink_matches(path, target)?
        {
            continue;
        }
        let before = root.inspect(path)?;
        if before.as_ref() == Some(&managed.node)
            || (managed.implicit && before.as_ref().is_some_and(Node::is_dir))
        {
            continue;
        }
        if managed.preserve {
            ensure!(
                before.as_ref().and_then(Node::capabilities).is_none(),
                "unexpected file capabilities on preserved config {path}"
            );
            ensure!(
                matches!(before, None | Some(Node::File { .. })),
                "config type changed at {path}"
            );
            preserved.push(path.to_owned());
            if let Some(after) = before
                .as_ref()
                .and_then(|local| with_vendor_metadata(local, &managed.node))
            {
                writes.insert(
                    path.to_owned(),
                    Write {
                        path: path.to_owned(),
                        before,
                        after: Some(after),
                        detach: false,
                        local_config: true,
                        config_move: None,
                    },
                );
            }
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
                detach: false,
                local_config: false,
                config_move: None,
            },
        );
    }
    Ok(writes)
}
