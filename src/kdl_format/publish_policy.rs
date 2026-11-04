use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use kdl::KdlDocument;

use crate::{
    attestation::MAX_BUILDERS,
    model::{Version, validate_digest, validate_name, validate_publisher},
};

use super::{Fields, document, one, schema};

pub(crate) struct PublishPolicy {
    pub publisher: String,
    pub builder_keys: BTreeSet<String>,
    pub minimum_builders: usize,
    pub recipes: BTreeMap<String, String>,
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
    })
}
