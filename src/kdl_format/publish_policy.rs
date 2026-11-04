use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use kdl::{KdlDocument, KdlNode};

use crate::{
    attestation::MAX_BUILDERS,
    model::{PackageSpec, Version, validate_digest, validate_name, validate_publisher},
};

use super::{Fields, document, one, schema};

pub(crate) struct PublishPolicy {
    pub publisher: String,
    pub builder_keys: BTreeSet<String>,
    pub minimum_builders: usize,
    pub recipes: BTreeMap<String, String>,
    pub profiles: BTreeMap<String, Profile>,
}

pub(crate) struct Profile {
    pub arch: String,
    pub facets: BTreeMap<String, bool>,
    pub packages: BTreeMap<String, Option<Version>>,
    pub upgrade_from: Option<String>,
}

fn profile(node: &KdlNode, publisher: &str) -> Result<(String, Profile)> {
    let fields = Fields::read(node, 1, &["architecture", "upgrade-from"], true)?;
    let name = fields.string_arg(0)?;
    validate_name(&name)?;
    let arch = fields.required_string("architecture")?;
    validate_name(&arch).context("invalid profile architecture")?;
    let upgrade_from = fields.string("upgrade-from")?;
    if let Some(hash) = &upgrade_from {
        validate_digest(hash).context("invalid profile upgrade catalog digest")?;
    }
    let mut packages = BTreeMap::new();
    let mut facets = BTreeMap::new();
    for child in node.children().into_iter().flat_map(KdlDocument::nodes) {
        match child.name().value() {
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
                ensure!(!facet.is_empty(), "empty profile facet");
                let enabled = fields.args[1]
                    .as_bool()
                    .context("facet must be #true or #false")?;
                ensure!(
                    facets.insert(facet.clone(), enabled).is_none(),
                    "duplicate profile facet {facet}"
                );
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
        },
    ))
}

pub(crate) fn parse(text: &str) -> Result<PublishPolicy> {
    let doc = document(text)?;
    schema(&doc)?;
    ensure!(
        doc.nodes().len() == 2,
        "expected schema-version and publisher nodes"
    );
    let node = one(&doc, "publisher")?;
    let fields = Fields::read(node, 1, &["minimum-builders"], true)?;
    let publisher = fields.string_arg(0)?;
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
                let fields = Fields::read(node, 1, &["version", "sha256"], false)?;
                let name = fields.string_arg(0)?;
                validate_name(&name)?;
                let version: Version = fields.required_string("version")?.parse()?;
                let digest = fields.required_string("sha256")?;
                validate_digest(&digest).context("invalid approved recipe digest")?;
                let id = format!("cps://{publisher}/{name}@{version}");
                ensure!(
                    recipes.insert(id.clone(), digest).is_none(),
                    "duplicate approval for {id}"
                );
            }
            "profile" => {
                let (name, profile) = profile(node, &publisher)?;
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
        profiles,
    })
}
