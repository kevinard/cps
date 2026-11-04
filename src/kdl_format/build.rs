use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use kdl::KdlDocument;

use crate::model::{FileDigest, validate_digest, validate_name, validate_path, validate_publisher};

use super::{Fields, document, one, schema};

pub(crate) struct BuildRecipe {
    pub arch: String,
    pub epoch: u32,
    pub catalog: String,
    pub packages: BTreeSet<String>,
    pub manifest: String,
    pub manifest_digest: String,
    pub sources: BTreeMap<String, FileDigest>,
    pub script: String,
}

pub(crate) fn parse(text: &str) -> Result<BuildRecipe> {
    let doc = document(text)?;
    schema(&doc)?;
    ensure!(
        doc.nodes().len() == 2,
        "expected schema-version and build nodes"
    );
    let build = one(&doc, "build")?;
    let fields = Fields::read(build, 0, &["architecture", "epoch"], true)?;
    let arch = fields.required_string("architecture")?;
    validate_publisher(&arch).context("invalid build architecture")?;
    let epoch = fields.integer("epoch")?.context("missing build epoch")?;
    let children = build.children().context("build requires a child block")?;

    let toolchain = one(children, "toolchain")?;
    let catalog = Fields::read(toolchain, 1, &[], true)?.string_arg(0)?;
    validate_digest(&catalog).context("invalid toolchain catalog")?;
    let mut packages = BTreeSet::new();
    for node in toolchain
        .children()
        .into_iter()
        .flat_map(KdlDocument::nodes)
    {
        ensure!(
            node.name().value() == "package",
            "expected a toolchain package"
        );
        let name = Fields::read(node, 1, &[], false)?.string_arg(0)?;
        validate_name(&name)?;
        ensure!(
            packages.insert(name.clone()),
            "duplicate toolchain package {name}"
        );
    }
    ensure!(
        !packages.is_empty(),
        "build requires at least one toolchain package"
    );

    let fields = Fields::read(one(children, "manifest")?, 1, &["sha256"], false)?;
    let manifest = fields.string_arg(0)?;
    validate_path(&manifest)?;
    let manifest_digest = fields.required_string("sha256")?;
    validate_digest(&manifest_digest).context("invalid manifest digest")?;

    let script = Fields::read(one(children, "script")?, 1, &[], false)?.string_arg(0)?;
    ensure!(
        !script.trim().is_empty() && !script.contains('\0'),
        "invalid build script"
    );

    let mut sources = BTreeMap::new();
    for node in children.nodes() {
        match node.name().value() {
            "toolchain" | "manifest" | "script" => {}
            "source" => {
                let fields = Fields::read(node, 1, &["sha256", "size"], false)?;
                let path = fields.string_arg(0)?;
                validate_path(&path)?;
                let sha256 = fields.required_string("sha256")?;
                validate_digest(&sha256).with_context(|| format!("invalid digest for {path}"))?;
                let size = fields.integer("size")?.context("missing source size")?;
                ensure!(
                    sources
                        .insert(path.clone(), FileDigest { sha256, size })
                        .is_none(),
                    "duplicate source {path}"
                );
            }
            name => bail!("unknown build node {name}"),
        }
    }
    Ok(BuildRecipe {
        arch,
        epoch,
        catalog,
        packages,
        manifest,
        manifest_digest,
        sources,
        script,
    })
}
