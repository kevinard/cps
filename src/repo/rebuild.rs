use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{FileType, RenameFlags, renameat_with};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::{
    attestation::{ATTESTATION_LIMIT, VerifiedBuild, verify_rebuild_inputs},
    image::root::Root,
    kdl_format::build,
    model::{FileDigest, digest_reader},
};

use super::{
    Catalog, MANIFEST_LIMIT, exact_publisher, read_catalog, read_current, read_key, read_limited,
    read_object, store, write_atomic,
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
    let catalog = selected_catalog(repo, publisher, &read_key(trust_key)?, id, pin)?;
    let inputs = catalog
        .build_inputs
        .get(id)
        .with_context(|| format!("no archived rebuild inputs for {id}"))?;
    let (published_hash, envelope) = admitted_build(repo, &catalog, id)?;
    let recipe = read_object(repo, OBJECTS, &inputs.recipe, build::RECIPE_LIMIT)?;
    let manifest = read_object(repo, OBJECTS, &inputs.manifest, MANIFEST_LIMIT)?;
    let definition =
        verify_rebuild_inputs(&envelope, id, &recipe, &manifest, published_hash)?.recipe;

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

/// Check a local rebuild against the version 3 attestation admitted by the signed catalog.
/// Returns the number of verified payloads.
pub fn verify_rebuild(
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    id: &str,
    pin: Option<&str>,
    bundle: &Path,
    witness_key: Option<&Path>,
) -> Result<usize> {
    let anchor = read_key(trust_key)?;
    let catalog = selected_catalog(repo, publisher, &anchor, id, pin)?;
    let (published_hash, envelope) = admitted_build(repo, &catalog, id)?;
    let bundle = Root::open(bundle)?;
    let recipe = read_limited(bundle.file("build.kdl")?, build::RECIPE_LIMIT)?;
    let manifest = read_limited(bundle.file("manifest.kdl")?, MANIFEST_LIMIT)?;
    let build = verify_rebuild_inputs(&envelope, id, &recipe, &manifest, published_hash)?;
    if let Some(inputs) = catalog.build_inputs.get(id) {
        ensure!(
            inputs.recipe == crate::model::digest(&recipe)
                && inputs.manifest == build.recipe.manifest_digest,
            "rebuild inputs differ from the signed archive for {id}"
        );
    }
    if let Some(witness_key) = witness_key {
        let key = read_key(witness_key)?;
        ensure!(
            !catalog
                .publisher_keys(&anchor)?
                .iter()
                .any(|publisher| publisher.as_slice() == key),
            "publisher keys cannot serve as rebuild witnesses"
        );
        let envelope = read_limited(bundle.file("attestation.json")?, ATTESTATION_LIMIT)?;
        build.check_witness(&envelope, &key)?;
    }
    verify_artifacts(&bundle.subdir("proto")?, &build.artifacts)?;
    Ok(build.artifacts.len())
}

fn selected_catalog(
    repo: &Path,
    publisher: &str,
    anchor: &[u8],
    id: &str,
    pin: Option<&str>,
) -> Result<Catalog> {
    ensure!(
        exact_publisher(id)? == publisher,
        "package ID belongs to a different publisher"
    );
    let hash = match pin {
        Some(hash) => hash.to_owned(),
        None => read_current(repo)?,
    };
    read_catalog(repo, &hash, publisher, anchor)
}

fn admitted_build<'a>(repo: &Path, catalog: &'a Catalog, id: &str) -> Result<(&'a str, Vec<u8>)> {
    let (published, attestation) = catalog
        .package_record(id)
        .with_context(|| format!("no published package {id}"))?;
    let attestation = attestation.with_context(|| format!("no build attestation for {id}"))?;
    let envelope = read_object(repo, "attestations", attestation, ATTESTATION_LIMIT)?;
    Ok((published.manifest_digest(), envelope))
}

fn verify_artifacts(proto: &Root, artifacts: &BTreeMap<String, FileDigest>) -> Result<()> {
    let directories: BTreeSet<_> = artifacts
        .keys()
        .flat_map(|path| Path::new(path).ancestors().skip(1))
        .filter(|path| !path.as_os_str().is_empty())
        .collect();
    proto.walk(None, |path, _, info| {
        let declared = match FileType::from_raw_mode(info.st_mode) {
            FileType::Directory => directories.contains(Path::new(path)),
            FileType::RegularFile => artifacts.contains_key(path),
            _ => false,
        };
        ensure!(
            declared,
            "unexpected entry in rebuilt payload tree at {path}"
        );
        Ok(true)
    })?;
    for (path, expected) in artifacts {
        let mut file = proto
            .file(path)
            .with_context(|| format!("opening rebuilt payload {path}"))?;
        ensure!(
            file.metadata()?.len() == expected.size,
            "rebuilt payload length differs at {path}"
        );
        let sha256 = digest_reader(file.by_ref().take(expected.size.saturating_add(1)))?;
        ensure!(
            sha256 == expected.sha256 && file.metadata()?.len() == expected.size,
            "rebuilt payload digest differs at {path}"
        );
    }
    Ok(())
}
