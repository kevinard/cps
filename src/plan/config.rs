use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::{CONFIG_UPDATES_DIR, Write, config_update_prefix};
use crate::{
    image::{Image, Managed, Settings, root::Root},
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
        },
    ))
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
