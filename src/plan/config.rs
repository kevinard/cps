use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::{
    CONFIG_UPDATES_DIR, ConfigMove, Write, config_update_prefix, paths::config_name, stage_config,
};
use crate::{
    image::{Image, Installed, Managed, Settings, State, root::Root},
    model::{Node, validate_path},
    verify::manifest_paths,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ConfigChoice {
    Local,
    Vendor,
}

#[derive(Debug)]
pub struct ConfigUpdate {
    pub path: String,
    pub revision: u64,
    pub vendor_copy: String,
    pub vendor_available: bool,
}

pub fn list_config_updates(root: &Path) -> Result<Vec<ConfigUpdate>> {
    let image = Image::open_query(root)?;
    image.require_clean()?;
    let state = image.state()?;
    let paths = manifest_paths(&state)?;
    ensure!(
        paths == state.paths,
        "path ownership index differs from installed manifests"
    );
    Ok(updates(&image.root, &state.settings, &paths)?
        .into_iter()
        .map(|(path, revision)| {
            let vendor_copy = vendor_copy(revision, &path);
            let vendor_available = image
                .root
                .inspect(&vendor_copy)
                .is_ok_and(|observed| observed.as_ref() == Some(&paths[&path].node));
            ConfigUpdate {
                path,
                revision,
                vendor_copy,
                vendor_available,
            }
        })
        .collect())
}

pub(super) fn resolve(
    root: &Root,
    settings: &mut Settings,
    paths: &BTreeMap<String, Managed>,
    path: &str,
    choice: ConfigChoice,
) -> Result<(u64, Write)> {
    root.offline_path()?;
    validate_path(path)?;
    let vendor = paths
        .get(path)
        .filter(|file| file.preserve)
        .with_context(|| format!("no managed preserved configuration at {path}"))?;
    let mut updates = updates(root, settings, paths)?;
    let revision = updates
        .remove(path)
        .with_context(|| format!("no pending configuration update for {path}"))?;
    let before = root.inspect(path)?;
    ensure!(
        matches!(
            before,
            None | Some(Node::File {
                capabilities: None,
                ..
            })
        ),
        "unexpected file type or capabilities on local configuration {path}"
    );
    let after = match choice {
        ConfigChoice::Local => before.clone(),
        ConfigChoice::Vendor => {
            let copy = vendor_copy(revision, path);
            ensure!(
                root.inspect(&copy)?.as_ref() == Some(&vendor.node),
                "staged vendor configuration {copy} differs from its installed manifest"
            );
            Some(vendor.node.clone())
        }
    };
    settings.config_updates = Some(updates);
    Ok((
        revision,
        Write {
            path: path.to_owned(),
            before,
            after,
            detach: false,
            local_config: false,
            config_move: None,
        },
    ))
}

pub(super) fn file_metadata(node: &Node) -> Option<(u32, u32, u32)> {
    match node {
        Node::File { mode, uid, gid, .. } => Some((*mode, *uid, *gid)),
        _ => None,
    }
}

pub(super) fn with_vendor_metadata(local: &Node, vendor: &Node) -> Option<Node> {
    if file_metadata(local) == file_metadata(vendor) {
        return None;
    }
    local_contents(local, vendor)
}

fn local_contents(local: &Node, vendor: &Node) -> Option<Node> {
    let Node::File {
        sha256,
        size,
        capabilities: None,
        ..
    } = local
    else {
        return None;
    };
    let (mode, uid, gid) = file_metadata(vendor)?;
    Some(Node::File {
        sha256: sha256.clone(),
        size: *size,
        mode,
        uid,
        gid,
        capabilities: None,
    })
}

pub(super) fn moves(
    state: &State,
    installed: &BTreeMap<String, Installed>,
    paths: &BTreeMap<String, Managed>,
) -> Result<BTreeMap<String, String>> {
    if !state
        .paths
        .values()
        .chain(paths.values())
        .any(|file| file.original_name.is_some())
    {
        return Ok(BTreeMap::new());
    }
    ensure!(
        manifest_paths(state)? == state.paths,
        "path ownership index differs from installed manifests"
    );
    let before = configuration_paths(&state.installed, &state.paths);
    let mut moves = BTreeMap::new();
    for (identity, to) in configuration_paths(installed, paths) {
        let Some(from) = before.get(&identity).copied().filter(|from| *from != to) else {
            continue;
        };
        ensure!(
            !state.paths.contains_key(to) && !paths.contains_key(from),
            "cannot move configuration from {from} to {to}: a package still claims the source or already claims the destination"
        );
        moves.insert(to.to_owned(), from.to_owned());
    }
    Ok(moves)
}

fn configuration_paths<'a>(
    installed: &'a BTreeMap<String, Installed>,
    paths: &'a BTreeMap<String, Managed>,
) -> BTreeMap<(&'a str, String), &'a str> {
    paths
        .iter()
        .filter(|(_, file)| file.preserve)
        .map(|(path, file)| {
            let owner = file.owners.first().expect("preserved file has an owner");
            let publisher = &installed[owner].published.manifest.publisher;
            let name = config_name(owner, path, file.original_name.as_deref());
            ((publisher.as_str(), name), path.as_str())
        })
        .collect()
}

pub(super) fn plan_move(
    root: &Root,
    settings: &mut Settings,
    revision: u64,
    (from, old): (&str, &Managed),
    (to, target): (&str, &Managed),
    writes: &mut BTreeMap<String, Write>,
) -> Result<()> {
    ensure!(
        root.inspect(to)?.is_none(),
        "unmanaged path already exists at {to}"
    );
    let observed = root.inspect(from)?;
    ensure!(
        matches!(
            observed,
            None | Some(Node::File {
                capabilities: None,
                ..
            })
        ),
        "unexpected file type or capabilities on local configuration {from}"
    );
    if let Some(updates) = &mut settings.config_updates {
        updates.remove(from);
    }
    let (after, local) = if observed.as_ref() == Some(&old.node) {
        (Some(target.node.clone()), None)
    } else {
        stage_config(root, settings, revision, to, &target.node, writes)?;
        let after = observed.as_ref().map(|local| {
            local_contents(local, &target.node).expect("preserved configuration is a regular file")
        });
        (after, observed.clone())
    };
    writes.insert(
        to.to_owned(),
        Write {
            path: to.to_owned(),
            before: None,
            after,
            detach: false,
            local_config: false,
            config_move: Some(ConfigMove {
                from: from.to_owned(),
                local,
            }),
        },
    );
    writes.insert(
        from.to_owned(),
        Write {
            path: from.to_owned(),
            before: observed,
            after: None,
            detach: false,
            local_config: false,
            config_move: None,
        },
    );
    Ok(())
}

fn updates(
    root: &Root,
    settings: &Settings,
    paths: &BTreeMap<String, Managed>,
) -> Result<BTreeMap<String, u64>> {
    if let Some(updates) = &settings.config_updates {
        for (path, revision) in updates {
            validate_path(path)?;
            ensure!(
                paths.get(path).is_some_and(|file| file.preserve)
                    && (1..=settings.revision).contains(revision),
                "invalid pending configuration update for {path}"
            );
        }
        return Ok(updates.clone());
    }

    // Recover old proposals only if their bytes and metadata match the
    // currently installed default. Prior revisions can contain older defaults.
    if root.inspect(CONFIG_UPDATES_DIR)?.is_none() {
        return Ok(BTreeMap::new());
    }
    let dir = root.dir(CONFIG_UPDATES_DIR)?;
    let mut revisions = Vec::new();
    for entry in fs::read_dir(Root::fd_path(&dir))? {
        let entry = entry?;
        let name = entry.file_name();
        if let Some(revision) = name.to_str().and_then(|name| name.parse::<u64>().ok())
            && name == revision.to_string().as_str()
            && (1..=settings.revision).contains(&revision)
            && entry.file_type()?.is_dir()
        {
            revisions.push(revision);
        }
    }
    revisions.sort_unstable_by(|a, b| b.cmp(a));
    let mut found = BTreeMap::new();
    for (path, file) in paths.iter().filter(|(_, file)| file.preserve) {
        if root.inspect(path)?.as_ref() == Some(&file.node) {
            continue;
        }
        if let Some(revision) = revisions.iter().copied().find(|revision| {
            root.inspect(&vendor_copy(*revision, path))
                .is_ok_and(|copy| copy.as_ref() == Some(&file.node))
        }) {
            found.insert(path.clone(), revision);
        }
    }
    Ok(found)
}

pub(super) fn vendor_copy(revision: u64, path: &str) -> String {
    format!("{}{path}", config_update_prefix(revision))
}
