use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, ensure};
use rustix::io::Errno;
use tempfile::NamedTempFile;

use crate::{
    build::sources::{self, copy_verified, publish_verified, verify_cached},
    image::root::Root,
    kdl_format::build::{self, BuildRecipe},
    model::FileDigest,
};

use super::{
    KeySpecs, Publication, exact_publisher, lock_repo, publication_base, publication_catalog,
    read_object, read_trust_root, rebuild, release, write_catalog,
};

pub(super) const OBJECTS: &str = "source-archives";

/// Attach the complete set of pinned sources to an admitted build.
pub fn archive_build_sources(
    repo: &Path,
    signing_key: &(impl KeySpecs + ?Sized),
    source_cache: &Path,
    id: &str,
) -> Result<(String, usize)> {
    let publisher = exact_publisher(id)?;
    Root::open(repo)?;
    let key = Publication::open(repo, signing_key)?;
    let _lock = lock_repo(repo)?;
    let current = publication_base(repo)?.context("repository has no published catalog")?;
    let mut catalog = publication_catalog(repo, publisher, &key, Some(&current))?;
    let inputs = rebuild::read_archived(repo, &catalog, id)?;
    let source_cache = Root::open(source_cache)?;
    let (changed, count) = stage(repo, &mut catalog, id, &inputs.recipe, Some(&source_cache))?;
    let hash = if changed {
        write_catalog(repo, catalog, &key)?
    } else {
        current
    };
    Ok((hash, count))
}

pub(super) fn stage(
    repo: &Path,
    catalog: &mut super::Catalog,
    id: &str,
    recipe: &BuildRecipe,
    source_cache: Option<&Root>,
) -> Result<(bool, usize)> {
    let pins = source_pins(recipe);
    let record = catalog
        .build_inputs
        .get_mut(id)
        .with_context(|| format!("no archived rebuild inputs for {id}"))?;
    if !pins.is_empty() {
        let destination = Root::open(repo)?.ensure_dir(OBJECTS)?;
        for pin in pins.values() {
            if !contains(&destination, pin)? {
                let file = source_cache
                    .with_context(|| {
                        format!(
                            "provide --source-cache to archive source {} for {id}",
                            pin.sha256
                        )
                    })?
                    .file(&format!("{}/{}", sources::OBJECTS, pin.sha256))
                    .with_context(|| format!("opening source cache object {}", pin.sha256))?;
                copy_source(file, &destination, pin)?;
            }
        }
    }
    let changed = !record.sources_archived;
    record.sources_archived = true;
    Ok((changed, pins.len()))
}

/// Populate a build source cache from a signed local archive or synchronized TUF mirror.
pub fn fetch_build_sources(
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    id: &str,
    pin: Option<&str>,
    source_cache: &Path,
) -> Result<usize> {
    let anchor = read_trust_root(trust_key)?;
    let catalog = rebuild::selected_catalog(repo, publisher, &anchor, id, pin)?;
    ensure!(
        catalog
            .build_inputs
            .get(id)
            .is_some_and(|inputs| inputs.sources_archived),
        "no source archive for {id} in the selected catalog"
    );
    let inputs = rebuild::read_archived(repo, &catalog, id)?;
    let pins = source_pins(&inputs.recipe);
    fs::create_dir_all(source_cache)?;
    let destination = Root::open(source_cache)?.ensure_dir(sources::OBJECTS)?;
    let repo_root = Root::open(repo)?;
    let mut missing = Vec::new();
    for pin in pins.values() {
        if contains(&destination, pin)? {
            continue;
        }
        match repo_root.file(&format!("{OBJECTS}/{}", pin.sha256)) {
            Ok(file) => copy_source(file, &destination, pin)?,
            Err(err) if err.downcast_ref::<Errno>() == Some(&Errno::NOENT) => missing.push(*pin),
            Err(err) => return Err(err),
        }
    }
    if !missing.is_empty() {
        release::fetch_source_archives(repo, publisher, &anchor, &destination, missing)?;
    }
    Ok(pins.len())
}

pub(super) fn archived_pins(repo: &Path, inputs: &rebuild::Digests) -> Result<Vec<FileDigest>> {
    let bytes = read_object(repo, rebuild::OBJECTS, &inputs.recipe, build::RECIPE_LIMIT)?;
    let recipe = build::parse(std::str::from_utf8(&bytes)?)?;
    Ok(source_pins(&recipe).into_values().cloned().collect())
}

fn source_pins(recipe: &BuildRecipe) -> BTreeMap<&str, &FileDigest> {
    recipe
        .sources
        .values()
        .map(|source| (source.pin.sha256.as_str(), &source.pin))
        .collect()
}

fn contains(root: &Root, pin: &FileDigest) -> Result<bool> {
    match root.file(&pin.sha256) {
        Ok(file) => {
            verify_cached(file, pin)?;
            Ok(true)
        }
        Err(err) if err.downcast_ref::<Errno>() == Some(&Errno::NOENT) => Ok(false),
        Err(err) => Err(err),
    }
}

fn copy_source(file: fs::File, destination: &Root, pin: &FileDigest) -> Result<()> {
    let mut temp = NamedTempFile::new_in(Root::fd_path(destination.as_file()))?;
    copy_verified(file, temp.as_file_mut(), pin, &pin.sha256)?;
    publish_verified(destination, temp, pin)
}
