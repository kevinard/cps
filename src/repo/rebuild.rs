use std::{
    fs::{self, File},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{RenameFlags, renameat_with};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::{
    attestation::{ATTESTATION_LIMIT, VerifiedBuild, check_rebuild_inputs},
    kdl_format::{build, parse_manifest},
};

use super::{
    Catalog, MANIFEST_LIMIT, exact_publisher, read_catalog, read_current, read_key, read_object,
    store, write_atomic,
};

pub(super) const OBJECTS: &str = "build-inputs";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Digests {
    recipe: String,
    manifest: String,
}

impl Digests {
    pub(super) fn objects(&self) -> [(&str, u64); 2] {
        [
            (&self.recipe, build::RECIPE_LIMIT),
            (&self.manifest, MANIFEST_LIMIT),
        ]
    }
}

pub(super) fn archive(
    repo: &Path,
    catalog: &mut Catalog,
    id: &str,
    evidence: &VerifiedBuild,
) -> Result<bool> {
    let inputs = Digests {
        recipe: store(repo, OBJECTS, &evidence.recipe_document)?,
        manifest: store(repo, OBJECTS, &evidence.manifest_document)?,
    };
    if let Some(previous) = catalog.build_inputs.get(id) {
        ensure!(
            previous == &inputs,
            "{id} already has different rebuild inputs"
        );
        return Ok(false);
    }
    catalog.build_inputs.insert(id.to_owned(), inputs);
    Ok(true)
}

/// Extract an authenticated build recipe, its manifest, and the admitted attestation.
/// Returns the path to the recipe in the new directory.
pub fn export_rebuild_inputs(
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    id: &str,
    pin: Option<&str>,
    output: &Path,
) -> Result<PathBuf> {
    ensure!(
        exact_publisher(id)? == publisher,
        "package ID belongs to a different publisher"
    );
    let hash = match pin {
        Some(hash) => hash.to_owned(),
        None => read_current(repo)?,
    };
    let catalog = read_catalog(repo, &hash, publisher, &read_key(trust_key)?)?;
    let inputs = catalog
        .build_inputs
        .get(id)
        .with_context(|| format!("no archived rebuild inputs for {id}"))?;
    let (published, attestation) = catalog
        .package_record(id)
        .context("missing package record for rebuild inputs")?;
    let envelope = read_object(
        repo,
        "attestations",
        attestation.context("missing build attestation for rebuild inputs")?,
        ATTESTATION_LIMIT,
    )?;
    let recipe = read_object(repo, OBJECTS, &inputs.recipe, build::RECIPE_LIMIT)?;
    let manifest = read_object(repo, OBJECTS, &inputs.manifest, MANIFEST_LIMIT)?;
    let definition = build::parse(std::str::from_utf8(&recipe)?)?;
    ensure!(
        definition.manifest_digest == inputs.manifest
            && parse_manifest(std::str::from_utf8(&manifest)?)?.id() == id,
        "archived recipe and manifest do not match for {id}"
    );
    check_rebuild_inputs(
        &envelope,
        id,
        &inputs.recipe,
        &manifest,
        published.manifest_digest(),
    )?;

    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let output_name = output
        .file_name()
        .context("missing rebuild output directory name")?;
    let directory = File::open(parent)?;
    let stage = TempDir::new_in(parent)?;
    let source = stage.path().join("source");
    let manifest_path = source.join(&definition.manifest);
    let manifest_parent = manifest_path
        .parent()
        .context("missing reviewed manifest directory")?;
    fs::create_dir_all(manifest_parent)?;
    let recipe_name = if Path::new(&definition.manifest).starts_with("build.kdl") {
        "rebuild.kdl"
    } else {
        "build.kdl"
    };
    write_atomic(&source.join(recipe_name), &recipe, true)?;
    write_atomic(&manifest_path, &manifest, true)?;
    write_atomic(&stage.path().join("attestation.json"), &envelope, true)?;
    for path in manifest_parent
        .ancestors()
        .take_while(|path| path.starts_with(stage.path()))
    {
        File::open(path)?.sync_all()?;
    }
    renameat_with(
        &directory,
        stage
            .path()
            .file_name()
            .context("missing rebuild staging directory")?,
        &directory,
        output_name,
        RenameFlags::NOREPLACE,
    )
    .context("publishing rebuild inputs directory")?;
    directory.sync_all()?;
    Ok(output.join("source").join(recipe_name))
}
