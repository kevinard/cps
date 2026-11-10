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
    attestation::{ATTESTATION_LIMIT, RebuildEvidence, VerifiedBuild, verify_rebuild_inputs},
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
    pub(super) recipe: String,
    /// The recipe's pinned templates, in recipe order.
    templates: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) sources_archived: bool,
}

impl Digests {
    pub(super) fn objects(&self) -> Vec<(&str, u64)> {
        std::iter::once((self.recipe.as_str(), build::RECIPE_LIMIT))
            .chain(
                self.templates
                    .iter()
                    .map(|template| (template.as_str(), MANIFEST_LIMIT)),
            )
            .collect()
    }

    pub(super) fn preserved_by(&self, next: &Self) -> bool {
        self.recipe == next.recipe
            && self.templates == next.templates
            && (!self.sources_archived || next.sources_archived)
    }

    fn matches(&self, recipe: &[u8], evidence: &RebuildEvidence) -> bool {
        self.recipe == crate::model::digest(recipe)
            && self
                .templates
                .iter()
                .eq(evidence.recipe.manifests.iter().map(|pin| &pin.sha256))
    }
}

/// Record the rebuild inputs of one output. Every output of a build records
/// the same recipe and templates.
pub(super) fn archive(
    repo: &Path,
    catalog: &mut Catalog,
    id: &str,
    evidence: &VerifiedBuild,
) -> Result<bool> {
    let inputs = Digests {
        recipe: store(repo, OBJECTS, &evidence.recipe_document)?,
        templates: evidence
            .templates
            .iter()
            .map(|input| store(repo, OBJECTS, &input.bytes))
            .collect::<Result<_>>()?,
        sources_archived: false,
    };
    if let Some(previous) = catalog.build_inputs.get(id) {
        ensure!(
            inputs.preserved_by(previous),
            "{id} already has different rebuild inputs"
        );
        return Ok(false);
    }
    catalog.build_inputs.insert(id.to_owned(), inputs);
    Ok(true)
}

pub(super) struct ArchivedInputs {
    recipe_document: Vec<u8>,
    envelope: Vec<u8>,
    pub(super) evidence: RebuildEvidence,
}

pub(super) fn read_archived(repo: &Path, catalog: &Catalog, id: &str) -> Result<ArchivedInputs> {
    let inputs = catalog
        .build_inputs
        .get(id)
        .with_context(|| format!("no archived rebuild inputs for {id}"))?;
    let admitted = admitted_build(repo, catalog, id)?;
    let recipe_document = read_object(repo, OBJECTS, &inputs.recipe, build::RECIPE_LIMIT)?;
    let evidence = verify_rebuild_inputs(&admitted.envelope, id, &recipe_document, |pin| {
        ensure!(
            inputs.templates.contains(&pin.sha256),
            "archived rebuild inputs omit {}",
            pin.path
        );
        read_object(repo, OBJECTS, &pin.sha256, MANIFEST_LIMIT)
    })?;
    ensure!(
        inputs.matches(&recipe_document, &evidence),
        "archived rebuild inputs differ from the recipe for {id}"
    );
    admitted.check(&evidence, id)?;
    Ok(ArchivedInputs {
        recipe_document,
        envelope: admitted.envelope,
        evidence,
    })
}

/// Extract an authenticated build recipe, its templates, and the admitted attestation.
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
    let inputs = read_archived(repo, &catalog, id)?;

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
    fs::create_dir(&source)?;
    let templates = &inputs.evidence.templates;
    let recipe_name = if templates
        .iter()
        .any(|input| Path::new(&input.path).starts_with("build.kdl"))
    {
        "rebuild.kdl"
    } else {
        "build.kdl"
    };
    write_atomic(&source.join(recipe_name), &inputs.recipe_document, true)?;
    let mut directories = BTreeSet::from([source.clone()]);
    for input in templates {
        let path = source.join(&input.path);
        let parent = path
            .parent()
            .context("missing reviewed manifest directory")?;
        fs::create_dir_all(parent)?;
        directories.extend(
            parent
                .ancestors()
                .take_while(|path| path.starts_with(&source))
                .map(Path::to_owned),
        );
        write_atomic(&path, &input.bytes, true)?;
    }
    write_atomic(
        &stage.path().join("attestation.json"),
        &inputs.envelope,
        true,
    )?;
    for path in directories.iter().rev() {
        File::open(path)?.sync_all()?;
    }
    File::open(stage.path())?.sync_all()?;
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

/// Check a local rebuild against the build attestation admitted by the signed catalog.
/// The admitted statement covers every output of the build, so the rebuild must
/// reproduce every output's manifest and payloads, and each output's catalog entry
/// must name this same attestation. Returns the number of verified payloads.
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
    let admitted = admitted_build(repo, &catalog, id)?;
    let bundle = Root::open(bundle)?;
    let recipe = read_limited(bundle.file("build.kdl")?, build::RECIPE_LIMIT)?;
    let templates = bundle.subdir(crate::build::TEMPLATES)?;
    let build = verify_rebuild_inputs(&admitted.envelope, id, &recipe, |pin| {
        read_limited(templates.file(&pin.path)?, MANIFEST_LIMIT)
            .with_context(|| format!("reading rebuilt template {}", pin.path))
    })?;
    if let Some(inputs) = catalog.build_inputs.get(id) {
        ensure!(
            inputs.matches(&recipe, &build),
            "rebuild inputs differ from the signed archive for {id}"
        );
    }
    for package in build.packages() {
        let sibling = admitted_build(repo, &catalog, package)
            .with_context(|| format!("checking {package} from the same build"))?;
        ensure!(
            sibling.attestation == admitted.attestation,
            "{package} names a different build attestation than {id}"
        );
        sibling.check(&build, package)?;
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
    let artifacts = build.check_outputs(&bundle)?;
    verify_artifacts(&bundle.subdir("proto")?, &artifacts)?;
    Ok(artifacts.len())
}

pub(super) fn selected_catalog(
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

/// The catalog record and admitted attestation of a published or withdrawn package.
struct Admitted<'a> {
    published: &'a str,
    attestation: &'a str,
    envelope: Vec<u8>,
}

impl Admitted<'_> {
    fn check(&self, evidence: &RebuildEvidence, id: &str) -> Result<()> {
        ensure!(
            evidence.published_manifest(id)? == self.published,
            "admitted build does not match the catalog manifest for {id}"
        );
        Ok(())
    }
}

fn admitted_build<'a>(repo: &Path, catalog: &'a Catalog, id: &str) -> Result<Admitted<'a>> {
    let (published, attestation) = catalog
        .package_record(id)
        .with_context(|| format!("no published package {id}"))?;
    let attestation = attestation.with_context(|| format!("no build attestation for {id}"))?;
    let envelope = read_object(repo, "attestations", attestation, ATTESTATION_LIMIT)?;
    Ok(Admitted {
        published: published.manifest_digest(),
        attestation,
        envelope,
    })
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
