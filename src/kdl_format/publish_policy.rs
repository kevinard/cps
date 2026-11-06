use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use kdl::{KdlDocument, KdlNode};

use crate::{
    attestation::MAX_BUILDERS,
    model::{
        PackageSpec, Version, validate_digest, validate_name, validate_path, validate_publisher,
    },
};

use super::{Fields, PublisherConfig, document, one, publisher_config, schema};

pub(crate) struct PublishPolicy {
    pub publisher: String,
    pub builder_keys: BTreeSet<String>,
    pub minimum_builders: usize,
    pub recipes: BTreeMap<String, RecipeApproval>,
    pub verify_toolchains: bool,
    pub archive_build_inputs: bool,
    pub require_source_archives: bool,
    pub profiles: BTreeMap<String, Profile>,
}

pub(crate) struct RecipeApproval {
    pub sha256: String,
    pub toolchain_config: Option<PathBuf>,
}

pub(crate) struct Profile {
    pub arch: String,
    pub facets: BTreeMap<String, bool>,
    pub packages: BTreeMap<String, Option<Version>>,
    pub upgrade_from: Option<String>,
    pub companions: Vec<Companion>,
    pub elf_library_dirs: Vec<String>,
    pub rollout_percentage: Option<u8>,
}

pub(crate) fn validate_elf_dirs(arch: &str, dirs: &[String]) -> Result<()> {
    ensure!(
        matches!(arch, "x86_64" | "aarch64"),
        "unsupported architecture for ELF checking {arch}"
    );
    ensure!(
        (1..=32).contains(&dirs.len()),
        "elf-check requires 1 to 32 library-dir entries"
    );
    let mut seen = BTreeSet::new();
    for dir in dirs {
        let relative = dir
            .strip_prefix('/')
            .context("library-dir must be absolute")?;
        validate_path(relative)?;
        ensure!(seen.insert(dir), "duplicate ELF library directory {dir}");
    }
    Ok(())
}

pub(crate) struct Companion {
    pub config: PublisherConfig,
    pub catalog: String,
    pub upgrade_from: Option<String>,
}

fn profile(node: &KdlNode, publisher: &str, parent: &Path) -> Result<(String, Profile)> {
    let fields = Fields::read(
        node,
        1,
        &["architecture", "upgrade-from", "rollout-percentage"],
        true,
    )?;
    let name = fields.string_arg(0)?;
    validate_name(&name)?;
    let arch = fields.required_string("architecture")?;
    validate_publisher(&arch).context("invalid profile architecture")?;
    let upgrade_from = fields.string("upgrade-from")?;
    if let Some(hash) = &upgrade_from {
        validate_digest(hash).context("invalid profile upgrade catalog digest")?;
    }
    let rollout_percentage: Option<u8> = fields.integer("rollout-percentage")?;
    ensure!(
        rollout_percentage.is_none_or(|percentage| percentage <= 100),
        "rollout-percentage must be between 0 and 100"
    );
    let mut packages = BTreeMap::new();
    let mut facets = BTreeMap::new();
    let mut companions = Vec::new();
    let mut elf_library_dirs = Vec::new();
    let mut names = BTreeSet::from([publisher.to_owned()]);
    for child in node.children().into_iter().flat_map(KdlDocument::nodes) {
        match child.name().value() {
            "publisher" => {
                let fields = Fields::read(child, 1, &["catalog", "upgrade-from"], true)?;
                let name = fields.string_arg(0)?;
                ensure!(
                    names.insert(name.clone()),
                    "duplicate profile publisher {name}"
                );
                let catalog = fields.required_string("catalog")?;
                validate_digest(&catalog).context("invalid companion catalog digest")?;
                let baseline = fields.string("upgrade-from")?;
                if let Some(hash) = &baseline {
                    ensure!(
                        upgrade_from.is_some(),
                        "companion upgrade-from requires a profile upgrade-from"
                    );
                    validate_digest(hash).context("invalid companion upgrade catalog digest")?;
                }
                companions.push(Companion {
                    config: publisher_config(child, name, parent)?,
                    catalog,
                    upgrade_from: baseline,
                });
            }
            "package" => {
                let selector = Fields::read(child, 1, &[], false)?.string_arg(0)?;
                let spec = PackageSpec::parse(&selector)?;
                ensure!(
                    spec.publisher.is_none_or(|name| name == publisher),
                    "profile package must belong to {publisher}"
                );
                ensure!(
                    packages
                        .insert(spec.name.to_owned(), spec.version)
                        .is_none(),
                    "duplicate profile package {}",
                    spec.name
                );
            }
            "facet" => {
                let fields = Fields::read(child, 2, &[], false)?;
                let facet = fields.string_arg(0)?;
                validate_publisher(&facet).context("invalid profile facet")?;
                let enabled = fields.args[1]
                    .as_bool()
                    .context("facet must be #true or #false")?;
                ensure!(
                    facets.insert(facet.clone(), enabled).is_none(),
                    "duplicate profile facet {facet}"
                );
            }
            "elf-check" => {
                ensure!(elf_library_dirs.is_empty(), "duplicate elf-check block");
                Fields::read(child, 0, &[], true)?;
                let directories = child
                    .children()
                    .context("elf-check requires a library-dir block")?;
                for node in directories.nodes() {
                    ensure!(
                        node.name().value() == "library-dir",
                        "expected library-dir under elf-check"
                    );
                    elf_library_dirs.push(Fields::read(node, 1, &[], false)?.string_arg(0)?);
                }
                validate_elf_dirs(&arch, &elf_library_dirs)?;
            }
            kind => bail!("unknown profile node {kind}"),
        }
    }
    ensure!(
        !packages.is_empty(),
        "profile {name} requires at least one package"
    );
    Ok((
        name,
        Profile {
            arch,
            facets,
            packages,
            upgrade_from,
            companions,
            elf_library_dirs,
            rollout_percentage,
        },
    ))
}

pub(crate) fn parse(text: &str, parent: &Path) -> Result<PublishPolicy> {
    let doc = document(text)?;
    schema(&doc)?;
    ensure!(
        doc.nodes().len() == 2,
        "expected schema-version and publisher nodes"
    );
    let node = one(&doc, "publisher")?;
    let fields = Fields::read(
        node,
        1,
        &[
            "minimum-builders",
            "verify-toolchains",
            "archive-build-inputs",
            "require-source-archives",
        ],
        true,
    )?;
    let publisher = fields.string_arg(0)?;
    let flag = |name: &str| -> Result<bool> {
        Ok(fields
            .properties
            .get(name)
            .map(|value| {
                value
                    .as_bool()
                    .with_context(|| format!("{name} must be #true or #false"))
            })
            .transpose()?
            .unwrap_or(false))
    };
    let verify_toolchains = flag("verify-toolchains")?;
    let archive_build_inputs = flag("archive-build-inputs")?;
    let require_source_archives = flag("require-source-archives")?;
    ensure!(
        !require_source_archives || archive_build_inputs,
        "require-source-archives requires archive-build-inputs"
    );
    let minimum_builders = fields.integer("minimum-builders")?.unwrap_or(1);
    ensure!(
        (1..=MAX_BUILDERS).contains(&minimum_builders),
        "minimum-builders must be between 1 and {MAX_BUILDERS}"
    );
    validate_publisher(&publisher)?;
    let mut builder_keys = BTreeSet::new();
    let mut recipes = BTreeMap::new();
    let mut profiles = BTreeMap::new();
    for node in node.children().into_iter().flat_map(KdlDocument::nodes) {
        match node.name().value() {
            "builder-key" => {
                let key = Fields::read(node, 1, &[], false)?.string_arg(0)?;
                ensure!(!key.is_empty(), "empty builder key path");
                ensure!(
                    builder_keys.insert(key.clone()),
                    "duplicate builder key {key}"
                );
            }
            "approve" => {
                let fields =
                    Fields::read(node, 1, &["version", "sha256", "toolchain-config"], false)?;
                let name = fields.string_arg(0)?;
                validate_name(&name)?;
                let version: Version = fields.required_string("version")?.parse()?;
                let digest = fields.required_string("sha256")?;
                validate_digest(&digest).context("invalid approved recipe digest")?;
                let id = format!("cps://{publisher}/{name}@{version}");
                let toolchain_config = fields
                    .string("toolchain-config")?
                    .map(|path| -> Result<PathBuf> {
                        ensure!(
                            !path.is_empty(),
                            "empty toolchain configuration path for {id}"
                        );
                        Ok(parent.join(path))
                    })
                    .transpose()?;
                ensure!(
                    !verify_toolchains || toolchain_config.is_some(),
                    "publish policy requires a toolchain configuration for {id}"
                );
                ensure!(
                    recipes
                        .insert(
                            id.clone(),
                            RecipeApproval {
                                sha256: digest,
                                toolchain_config
                            }
                        )
                        .is_none(),
                    "duplicate approval for {id}"
                );
            }
            "profile" => {
                let (name, profile) = profile(node, &publisher, parent)?;
                ensure!(
                    profiles.insert(name.clone(), profile).is_none(),
                    "duplicate profile {name}"
                );
            }
            name => bail!("unknown publish policy node {name}"),
        }
    }
    ensure!(
        !builder_keys.is_empty(),
        "publish policy requires a builder key"
    );
    Ok(PublishPolicy {
        publisher,
        builder_keys,
        minimum_builders,
        recipes,
        verify_toolchains,
        archive_build_inputs,
        require_source_archives,
        profiles,
    })
}
