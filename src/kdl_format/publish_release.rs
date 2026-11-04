use std::{collections::BTreeSet, path::PathBuf};

use anyhow::{Result, bail, ensure};
use kdl::KdlDocument;

use crate::{
    attestation::MAX_BUILDERS,
    model::{PackageSpec, validate_digest, validate_path, validate_publisher},
    repo::BuildWitness,
};

use super::{Fields, document, one, schema};

pub(crate) struct Release {
    pub publisher: String,
    pub base: Option<String>,
    pub changes: Vec<Change>,
}

pub(crate) enum Change {
    Bundle {
        id: String,
        path: PathBuf,
        builder_key: PathBuf,
        recipe: String,
        witnesses: Vec<BuildWitness>,
    },
    Withdraw(String),
}

fn path(value: String) -> Result<PathBuf> {
    validate_path(&value)?;
    Ok(value.into())
}

pub(crate) fn parse(text: &str) -> Result<Release> {
    let doc = document(text)?;
    schema(&doc)?;
    ensure!(
        doc.nodes().len() == 2,
        "expected schema-version and release nodes"
    );
    let release = one(&doc, "release")?;
    let fields = Fields::read(release, 1, &["base"], true)?;
    let publisher = fields.string_arg(0)?;
    validate_publisher(&publisher)?;
    let base = fields.required_string("base")?;
    let base = if base == "empty" {
        None
    } else {
        validate_digest(&base)?;
        Some(base)
    };
    let mut ids = BTreeSet::new();
    let mut changes = Vec::new();
    for node in release.children().into_iter().flat_map(KdlDocument::nodes) {
        let kind = node.name().value();
        let fields = match kind {
            "bundle" => Fields::read(node, 1, &["path", "builder-key", "recipe-sha256"], true)?,
            "withdraw" => Fields::read(node, 1, &[], false)?,
            _ => bail!("unknown release action {kind}"),
        };
        let id = fields.string_arg(0)?;
        let spec = PackageSpec::parse(&id)?;
        ensure!(
            spec.publisher == Some(&publisher) && spec.version.is_some(),
            "release action requires a full package ID for {publisher}: {id}"
        );
        ensure!(ids.insert(id.clone()), "duplicate release action for {id}");
        if kind == "withdraw" {
            changes.push(Change::Withdraw(id));
            continue;
        }
        let recipe = fields.required_string("recipe-sha256")?;
        validate_digest(&recipe)?;
        let mut witnesses = Vec::new();
        for witness in node.children().into_iter().flat_map(KdlDocument::nodes) {
            ensure!(
                witness.name().value() == "witness",
                "expected a witness node"
            );
            ensure!(
                witnesses.len() + 1 < MAX_BUILDERS,
                "too many build witnesses"
            );
            let fields = Fields::read(witness, 1, &["key"], false)?;
            witnesses.push(BuildWitness {
                key: path(fields.required_string("key")?)?,
                attestation: path(fields.string_arg(0)?)?,
            });
        }
        changes.push(Change::Bundle {
            id,
            path: path(fields.required_string("path")?)?,
            builder_key: path(fields.required_string("builder-key")?)?,
            recipe,
            witnesses,
        });
    }
    ensure!(!changes.is_empty(), "release has no changes");
    Ok(Release {
        publisher,
        base,
        changes,
    })
}
